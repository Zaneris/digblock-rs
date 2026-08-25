using Microsoft.EntityFrameworkCore;

namespace DigBlock.Master.Data;

/// <summary>The master's durable state: accounts, their provider logins, their sessions, and the
/// host:port claims game servers hold. Everything ephemeral stays out on purpose — join tokens,
/// pending auth codes, registration temp-bans and per-server liveness live in memory, because
/// losing them on a restart is already accepted. SQLite is the current provider, not an assumption:
/// no raw SQL and no SQLite-only types, so <c>UseSqlite</c> is the one line to change.</summary>
public sealed class MasterDb(DbContextOptions<MasterDb> options) : DbContext(options)
{
    public DbSet<Account> Accounts => Set<Account>();
    public DbSet<ProviderLink> ProviderLinks => Set<ProviderLink>();
    public DbSet<NameReservation> NameReservations => Set<NameReservation>();
    public DbSet<Session> Sessions => Set<Session>();
    public DbSet<ServerClaim> ServerClaims => Set<ServerClaim>();
    public DbSet<SkillTotal> SkillTotals => Set<SkillTotal>();

    protected override void OnModelCreating(ModelBuilder b)
    {
        b.Entity<Account>(e =>
        {
            e.HasKey(a => a.Id);
            e.Property(a => a.DisplayName).HasMaxLength(Names.MaxLength);
            e.Property(a => a.DisplayNameNormalized).HasMaxLength(Names.MaxLength);
            e.Property(a => a.BanReason).HasMaxLength(200);
            // Nulls compare distinct in a unique index (SQLite and Postgres
            // both), so unnamed accounts need no filter here.
            e.HasIndex(a => a.DisplayNameNormalized).IsUnique();
        });

        b.Entity<ProviderLink>(e =>
        {
            e.HasKey(p => p.Id);
            e.Property(p => p.Subject).HasMaxLength(128);
            // The login itself. Two accounts can never share one provider
            // identity, so a returning player always lands on their account
            // rather than silently getting a fresh one.
            e.HasIndex(p => new { p.Provider, p.Subject }).IsUnique();
            e.HasOne(p => p.Account)
                .WithMany(a => a.Providers)
                .HasForeignKey(p => p.AccountId)
                .OnDelete(DeleteBehavior.Cascade);
        });

        b.Entity<NameReservation>(e =>
        {
            e.HasKey(n => n.NameNormalized);
            e.Property(n => n.NameNormalized).HasMaxLength(Names.MaxLength);
            // Swept by expiry, so the sweep is an index seek, not a scan.
            e.HasIndex(n => n.ReleasedUtc);
        });

        b.Entity<Session>(e =>
        {
            e.HasKey(s => s.Id);
            e.Property(s => s.TokenHash).HasMaxLength(32);
            e.Property(s => s.Platform).HasMaxLength(16);
            // Every authenticated request is a lookup by this hash.
            e.HasIndex(s => s.TokenHash).IsUnique();
            e.HasIndex(s => s.ChainId);
            e.HasIndex(s => s.ExpiresUtc);
            e.HasOne(s => s.Account)
                .WithMany(a => a.Sessions)
                .HasForeignKey(s => s.AccountId)
                .OnDelete(DeleteBehavior.Cascade);
        });

        b.Entity<ServerClaim>(e =>
        {
            e.HasKey(c => c.Id);
            e.Property(c => c.Host).HasMaxLength(255);
            e.Property(c => c.Name).HasMaxLength(ServerRegistry.MaxNameLength);
            e.Property(c => c.KeyHash).HasMaxLength(32);
            // One claim per address — this uniqueness IS the stickiness.
            e.HasIndex(c => new { c.Host, c.Port }).IsUnique();
            e.HasIndex(c => c.KeyHash).IsUnique();
        });

        b.Entity<SkillTotal>(e =>
        {
            e.HasKey(t => new { t.AccountId, t.Skill });
            // A per-skill board is a walk down this index.
            e.HasIndex(t => new { t.Skill, t.Xp });
            e.HasOne(t => t.Account)
                .WithMany()
                .HasForeignKey(t => t.AccountId)
                .OnDelete(DeleteBehavior.Cascade);
        });
    }
}
