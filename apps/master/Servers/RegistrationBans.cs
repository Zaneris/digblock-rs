using System.Collections.Concurrent;
using System.Net;

namespace DigBlock.Master.Servers;

/// <summary>Short bans on registering, applied to the source address of a
/// takeover attempt: re-POSTing a live server's host, port and certHash as
/// your own to inherit its id and key. Refusing that is the fix; the ban is
/// what makes a loop re-firing on every re-registration pointless. In memory
/// on purpose — persisting writes attacker-controlled rows on demand.</summary>
public sealed class RegistrationBans
{
    /// <summary>First offence. Doubles per repeat while the ban is still
    /// remembered, so a persistent attacker backs off fast and a one-off
    /// misconfiguration barely notices.</summary>
    public static readonly TimeSpan BaseDuration = TimeSpan.FromMinutes(15);

    public static readonly TimeSpan MaxDuration = TimeSpan.FromHours(6);

    private sealed record Entry(DateTimeOffset Until, TimeSpan Duration, int Strikes);

    private readonly ConcurrentDictionary<IPAddress, Entry> _bans = new();

    public bool IsBanned(IPAddress? address) =>
        address is not null
        && _bans.TryGetValue(address, out var e)
        && e.Until > DateTimeOffset.UtcNow;

    /// <summary>Ban (or extend) an address. Returns how long for, so the
    /// caller can log it.</summary>
    public TimeSpan Ban(IPAddress address)
    {
        var now = DateTimeOffset.UtcNow;
        var entry = _bans.AddOrUpdate(
            address,
            _ => new Entry(now + BaseDuration, BaseDuration, 1),
            (_, prev) =>
            {
                var next = prev.Until > now
                    ? TimeSpan.FromTicks(Math.Min(prev.Duration.Ticks * 2, MaxDuration.Ticks))
                    : BaseDuration;
                return new Entry(now + next, next, prev.Strikes + 1);
            });
        return entry.Duration;
    }

    /// <summary>Drop bans that lapsed a while ago. Kept past expiry on
    /// purpose — the strike count is what makes a repeat offender escalate,
    /// so forgetting it the instant a ban lifts would reset the ladder.</summary>
    public void SweepExpired()
    {
        var cutoff = DateTimeOffset.UtcNow - MaxDuration;
        foreach (var (ip, entry) in _bans)
        {
            if (entry.Until < cutoff)
                _bans.TryRemove(ip, out _);
        }
    }
}
