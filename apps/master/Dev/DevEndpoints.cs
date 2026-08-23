using System.Security.Cryptography;
using System.Text;
using DigBlock.Master.Accounts;
using DigBlock.Master.Data;
using Microsoft.EntityFrameworkCore;
using Microsoft.Extensions.Options;

namespace DigBlock.Master.Dev;

/// <summary>Sign-in without a provider, for the things that cannot open a
/// browser: <c>just play</c>, the headless bots, and the Rust e2e suite. A real
/// hole, so it is gated three ways: the option must be set, the environment
/// must be Development, and setting the option outside Development kills the
/// process at boot (Program.cs). A misconfiguration is loud, never silent.</summary>
public static class DevEndpoints
{
    /// <summary>The account id for a dev subject, deterministic on purpose:
    /// <c>just play</c> passes the admin's id to the game server before anybody
    /// has signed in, so it can be a justfile constant. Built via the textual
    /// form, not <c>new Guid(byte[])</c>, which is mixed-endian on its first
    /// three fields. <c>dev_account_ids_are_pinned</c> holds the derivation.</summary>
    public static Guid AccountId(string subject)
    {
        var digest = SHA256.HashData(Encoding.UTF8.GetBytes("digblock-dev-account:" + subject));
        var b = digest[..16];
        b[6] = (byte)((b[6] & 0x0F) | 0x80); // UUID version 8 (custom)
        b[8] = (byte)((b[8] & 0x3F) | 0x80); // RFC 4122 variant
        return Guid.ParseExact(Convert.ToHexStringLower(b), "N");
    }

    public static void MapDevEndpoints(this WebApplication app)
    {
        app.MapPost("/api/dev/signin", async (
            DevSignInRequest req, MasterDb db, AccountService accounts,
            IOptions<MasterOptions> opts, CancellationToken ct) =>
        {
            var subject = req.Subject?.Trim();
            if (string.IsNullOrEmpty(subject) || subject.Length > 64)
                return Results.BadRequest(new ApiError("subject required"));

            var id = AccountId(subject);
            var account = await db.Accounts.FindAsync([id], ct);
            if (account is null)
            {
                account = new Account
                {
                    Id = id,
                    CreatedUtc = DateTimeOffset.UtcNow,
                    LastSeenUtc = DateTimeOffset.UtcNow,
                    Status = AccountStatus.Active,
                };
                db.Accounts.Add(account);
                await db.SaveChangesAsync(ct);
            }

            // Claim the subject as the display name when it is a legal one
            // and free, so scripted flows get a joinable account in a single
            // call. A subject that is not a legal name (the bots use long
            // ones) just leaves the account unnamed for the caller to name.
            if (account.DisplayName is null && Names.IsValid(subject))
                await accounts.SetNameAsync(account, subject, ct);

            var token = await accounts.MintSessionAsync(account, req.Platform ?? "dev", null, ct);
            return Results.Ok(new SessionResponse(
                account.Id, account.DisplayName, token,
                (int)TimeSpan.FromDays(opts.Value.Account.SessionDays).TotalSeconds));
        }).RequireCors(Cors.Client);
    }
}
