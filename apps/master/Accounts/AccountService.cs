using DigBlock.Master.Data;
using Microsoft.EntityFrameworkCore;
using Microsoft.Extensions.Options;

namespace DigBlock.Master.Accounts;

/// <summary>What a name change can come back as. The caller maps these to
/// status codes; the wording players see comes from one place, so the game
/// client and the web pages cannot describe the same refusal differently.</summary>
public enum NameResult
{
    Ok,

    /// <summary>Fails <see cref="Names.IsValid"/>.</summary>
    Invalid,

    /// <summary>Held by another account, or inside another account's
    /// release quarantine.</summary>
    Taken,

    /// <summary>Inside this account's own rename cooldown.</summary>
    Cooldown,
}

/// <summary>Every read and write of account and session state. Endpoints
/// call this and never touch <see cref="MasterDb"/> directly, so the rules
/// that make an account trustworthy — one provider identity per account,
/// unique names, session rotation with reuse detection — live in exactly
/// one place.</summary>
public sealed class AccountService(MasterDb db, IOptions<MasterOptions> options, ILogger<AccountService> log)
{
    private AccountOptions Opts => options.Value.Account;

    // ---- sign-in ----------------------------------------------------------

    /// <summary>Resolve the account behind a provider identity, creating one
    /// the first time we see it. The (provider, subject) unique index is
    /// what guarantees a returning player lands on their own account rather
    /// than quietly getting a fresh one.</summary>
    public async Task<Account> FindOrCreateAsync(AuthProvider provider, string subject, CancellationToken ct)
    {
        var link = await db.ProviderLinks
            .Include(p => p.Account)
            .SingleOrDefaultAsync(p => p.Provider == provider && p.Subject == subject, ct);
        if (link is not null)
            return link.Account;

        var now = DateTimeOffset.UtcNow;
        var account = new Account
        {
            Id = Guid.NewGuid(),
            CreatedUtc = now,
            LastSeenUtc = now,
            Status = AccountStatus.Active,
        };
        db.Accounts.Add(account);
        db.ProviderLinks.Add(new ProviderLink
        {
            AccountId = account.Id,
            Provider = provider,
            Subject = subject,
            LinkedUtc = now,
        });
        await db.SaveChangesAsync(ct);
        log.LogInformation("created account {AccountId} via {Provider}", account.Id, provider);
        return account;
    }

    /// <summary>Attach another provider to an account already signed in, so
    /// a player who started with Google can add Facebook and keep one
    /// character. No-op when the link already exists.</summary>
    public async Task LinkProviderAsync(Account account, AuthProvider provider, string subject, CancellationToken ct)
    {
        var exists = await db.ProviderLinks
            .AnyAsync(p => p.AccountId == account.Id && p.Provider == provider, ct);
        if (exists)
            return;
        db.ProviderLinks.Add(new ProviderLink
        {
            AccountId = account.Id,
            Provider = provider,
            Subject = subject,
            LinkedUtc = DateTimeOffset.UtcNow,
        });
        await db.SaveChangesAsync(ct);
    }

    // ---- sessions ---------------------------------------------------------

    /// <summary>Mint a session and return its raw token — the only moment
    /// the token exists outside the client, since only its hash is stored.
    /// <paramref name="chainId"/> continues an existing rotation chain
    /// (refresh) or starts one when null (fresh sign-in).</summary>
    public async Task<string> MintSessionAsync(
        Account account, string? platform, Guid? chainId, CancellationToken ct)
    {
        var token = Secrets.New();
        var now = DateTimeOffset.UtcNow;
        db.Sessions.Add(new Session
        {
            Id = Guid.NewGuid(),
            AccountId = account.Id,
            TokenHash = Secrets.Hash(token),
            ChainId = chainId ?? Guid.NewGuid(),
            CreatedUtc = now,
            LastSeenUtc = now,
            ExpiresUtc = now.AddDays(Opts.SessionDays),
            Platform = string.IsNullOrWhiteSpace(platform) ? null : platform[..Math.Min(platform.Length, 16)],
        });
        account.LastSeenUtc = now;
        await db.SaveChangesAsync(ct);
        return token;
    }

    /// <summary>Resolve a bearer token to its live session, or null. A
    /// revoked, expired, or unknown token is deliberately indistinguishable
    /// to the caller.</summary>
    public async Task<Session?> AuthenticateAsync(string? token, CancellationToken ct)
    {
        if (string.IsNullOrEmpty(token))
            return null;
        var hash = Secrets.Hash(token);
        var session = await db.Sessions
            .Include(s => s.Account)
            .SingleOrDefaultAsync(s => s.TokenHash == hash, ct);
        if (session is null)
            return null;
        if (session.ExpiresUtc <= DateTimeOffset.UtcNow)
            return null;
        if (session.RevokedUtc is not null)
        {
            // Only a ROTATED token replayed is a security event: a refresh superseded it, so a
            // second copy exists, and we cannot tell the thief from the victim — the whole chain
            // dies and both sign in again. A signed-out or cascade-revoked token is an ordinary
            // stale client: refuse it without crying wolf, and without re-killing a dead chain.
            if (session.RevokedReason == SessionRevocation.Rotated)
            {
                log.LogWarning(
                    "rotated session replayed for account {AccountId}; revoking chain", session.AccountId);
                await RevokeChainAsync(session.ChainId, SessionRevocation.Cascade, ct);
            }
            return null;
        }
        if (session.Account.Status != AccountStatus.Active)
            return null;
        return session;
    }

    /// <summary>Rotate: mint the successor, revoke the presented token. The
    /// old token staying revoked rather than deleted is what makes replay
    /// detectable in <see cref="AuthenticateAsync"/>.</summary>
    public async Task<string> RefreshAsync(Session session, string? platform, CancellationToken ct)
    {
        session.RevokedUtc = DateTimeOffset.UtcNow;
        session.RevokedReason = SessionRevocation.Rotated;
        session.LastSeenUtc = DateTimeOffset.UtcNow;
        return await MintSessionAsync(session.Account, platform ?? session.Platform, session.ChainId, ct);
    }

    public async Task RevokeAsync(Session session, CancellationToken ct)
    {
        session.RevokedUtc = DateTimeOffset.UtcNow;
        session.RevokedReason = SessionRevocation.SignedOut;
        await db.SaveChangesAsync(ct);
    }

    /// <summary>Revoke every session in a rotation chain.</summary>
    public async Task RevokeChainAsync(Guid chainId, SessionRevocation reason, CancellationToken ct)
    {
        var now = DateTimeOffset.UtcNow;
        await db.Sessions
            .Where(s => s.ChainId == chainId && s.RevokedUtc == null)
            .ExecuteUpdateAsync(s => s
                .SetProperty(x => x.RevokedUtc, now)
                .SetProperty(x => x.RevokedReason, reason), ct);
    }

    /// <summary>Sign out everywhere — the account screen's big red button,
    /// and the honest answer to "I think someone has my session".</summary>
    public async Task RevokeAllAsync(Guid accountId, CancellationToken ct)
    {
        var now = DateTimeOffset.UtcNow;
        await db.Sessions
            .Where(s => s.AccountId == accountId && s.RevokedUtc == null)
            .ExecuteUpdateAsync(s => s
                .SetProperty(x => x.RevokedUtc, now)
                .SetProperty(x => x.RevokedReason, SessionRevocation.SignedOut), ct);
    }

    // ---- names ------------------------------------------------------------

    public async Task<Account?> FindByNameAsync(string name, CancellationToken ct)
    {
        var norm = Names.Normalize(name);
        return await db.Accounts.SingleOrDefaultAsync(a => a.DisplayNameNormalized == norm, ct);
    }

    /// <summary>Could <paramref name="asker"/> take this name right now?
    /// Answers the live-name index and the quarantine together, because a
    /// name free in one and held in the other is not available.</summary>
    public async Task<bool> IsNameAvailableAsync(string name, Guid? asker, CancellationToken ct)
    {
        if (!Names.IsValid(name))
            return false;
        var norm = Names.Normalize(name);
        var holder = await db.Accounts
            .Where(a => a.DisplayNameNormalized == norm)
            .Select(a => (Guid?)a.Id)
            .SingleOrDefaultAsync(ct);
        if (holder is not null)
            return holder == asker;
        return !await IsQuarantinedAsync(norm, asker, ct);
    }

    private async Task<bool> IsQuarantinedAsync(string norm, Guid? asker, CancellationToken ct)
    {
        var cutoff = DateTimeOffset.UtcNow.AddDays(-Opts.NameQuarantineDays);
        var reservation = await db.NameReservations.SingleOrDefaultAsync(n => n.NameNormalized == norm, ct);
        if (reservation is null || reservation.ReleasedUtc <= cutoff)
            return false;
        // Your own released name is always yours to take back — the
        // quarantine exists to stop other people grabbing it, not to punish
        // a change of mind.
        return reservation.AccountId != asker;
    }

    /// <summary>Set or change the account's display name.</summary>
    public async Task<(NameResult Result, DateTimeOffset? RetryAfterUtc)> SetNameAsync(
        Account account, string name, CancellationToken ct)
    {
        if (!Names.IsValid(name))
            return (NameResult.Invalid, null);

        var norm = Names.Normalize(name);
        var now = DateTimeOffset.UtcNow;
        var previous = account.DisplayNameNormalized;

        // Re-casing your own name is free and is not a change: nobody else
        // can be confused by "zaneris" becoming "Zaneris".
        if (previous == norm)
        {
            account.DisplayName = name;
            await db.SaveChangesAsync(ct);
            return (NameResult.Ok, null);
        }

        if (previous is not null && account.DisplayNameChangedUtc is { } changed)
        {
            var until = changed.AddDays(Opts.NameChangeCooldownDays);
            if (until > now)
                return (NameResult.Cooldown, until);
        }

        var takenBy = await db.Accounts
            .Where(a => a.DisplayNameNormalized == norm)
            .Select(a => (Guid?)a.Id)
            .SingleOrDefaultAsync(ct);
        if (takenBy is not null)
            return (NameResult.Taken, null);
        if (await IsQuarantinedAsync(norm, account.Id, ct))
            return (NameResult.Taken, null);

        if (previous is not null)
        {
            // Park the name being left behind, and start the cooldown. The
            // clock only starts here, which is what makes the first change
            // after sign-up free.
            var parked = await db.NameReservations.SingleOrDefaultAsync(n => n.NameNormalized == previous, ct);
            if (parked is null)
                db.NameReservations.Add(new NameReservation
                {
                    NameNormalized = previous,
                    AccountId = account.Id,
                    ReleasedUtc = now,
                });
            else
            {
                parked.AccountId = account.Id;
                parked.ReleasedUtc = now;
            }
            account.DisplayNameChangedUtc = now;
        }

        // Claiming a name clears any reservation on it (ours, or one whose
        // quarantine has expired — the checks above already allowed it).
        var claimed = await db.NameReservations.SingleOrDefaultAsync(n => n.NameNormalized == norm, ct);
        if (claimed is not null)
            db.NameReservations.Remove(claimed);

        account.DisplayName = name;
        account.DisplayNameNormalized = norm;

        try
        {
            await db.SaveChangesAsync(ct);
        }
        catch (DbUpdateException)
        {
            // Lost the race to the unique index between the check above and
            // the write. Report it as taken, which is what it now is.
            db.ChangeTracker.Clear();
            return (NameResult.Taken, null);
        }
        return (NameResult.Ok, null);
    }

    // ---- bans -------------------------------------------------------------

    /// <summary>Is this account barred from joining anything at all? A
    /// global ban is the master's own sanction and is separate from the
    /// per-server bans a game server keeps in its <c>bans.json</c>.</summary>
    public static bool IsGloballyBanned(Account account) =>
        account.Status == AccountStatus.Banned
        && (account.BanUntilUtc is null || account.BanUntilUtc > DateTimeOffset.UtcNow);
}
