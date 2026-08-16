using DigBlock.Master.Auth;
using DigBlock.Master.Servers;
using Microsoft.Extensions.Options;

namespace DigBlock.Master;

/// <summary>Periodically re-probes registered servers (skipping ones that
/// heartbeated within the interval) and removes them after MaxStrikes
/// consecutive failures. Also sweeps every expiring in-memory store — join
/// tokens, and the two halves of a pending sign-in.</summary>
public sealed class HealthChecker(
    ServerRegistry registry,
    TokenStore tokens,
    AuthRequestStore authRequests,
    RegistrationBans bans,
    IReachabilityProbe probe,
    IOptions<MasterOptions> options,
    ILogger<HealthChecker> log) : BackgroundService
{
    protected override async Task ExecuteAsync(CancellationToken ct)
    {
        var interval = TimeSpan.FromSeconds(options.Value.Probe.IntervalSeconds);
        using var timer = new PeriodicTimer(interval);
        while (await timer.WaitForNextTickAsync(ct))
        {
            try
            {
                await SweepAsync(interval, ct);
            }
            catch (OperationCanceledException) when (ct.IsCancellationRequested)
            {
                throw;
            }
            catch (Exception e)
            {
                log.LogError(e, "health sweep failed");
            }
        }
    }

    private async Task SweepAsync(TimeSpan interval, CancellationToken ct)
    {
        tokens.SweepExpired();
        authRequests.SweepExpired();
        bans.SweepExpired();

        var maxStrikes = options.Value.Probe.MaxStrikes;
        var idleTimeout = TimeSpan.FromSeconds(options.Value.Registry.IdleTimeoutSeconds);
        var now = DateTimeOffset.UtcNow;

        // Silence is its own answer. A server that has stopped heartbeating
        // is gone, and there is nothing a probe would add — so de-list it
        // outright rather than spending three strikes discovering it. This
        // runs BEFORE the probe sweep so a dead server never costs a dial.
        foreach (var entry in registry.Snapshot())
        {
            if (now - entry.LastSeenUtc >= idleTimeout && registry.Remove(entry.Id))
                log.LogInformation("de-listed {Name} ({Host}:{Port}): no heartbeat for {Seconds}s",
                    entry.Name, entry.Host, entry.Port, (int)idleTimeout.TotalSeconds);
        }

        // A recent heartbeat is already proof of life — skip those probes.
        var due = registry.Snapshot().Where(e => now - e.LastSeenUtc >= interval).ToList();
        using var gate = new SemaphoreSlim(8);
        await Task.WhenAll(due.Select(async entry =>
        {
            await gate.WaitAsync(ct);
            try
            {
                var result = await probe.ProbeAsync(entry.ProbeHost, entry.Port, entry.CertHash, ct);
                if (result.Reachable)
                {
                    entry.Strikes = 0;
                    entry.LastSeenUtc = DateTimeOffset.UtcNow;
                }
                else if (++entry.Strikes >= maxStrikes)
                {
                    if (registry.Remove(entry.Id))
                        log.LogWarning("removed {Name} ({Host}:{Port}) after {Strikes} failed probes: {Reason}",
                            entry.Name, entry.Host, entry.Port, entry.Strikes, result.ReasonString);
                }
                else
                {
                    log.LogInformation("probe failed for {Name} ({Host}:{Port}), strike {Strikes}/{Max}: {Reason}",
                        entry.Name, entry.Host, entry.Port, entry.Strikes, maxStrikes, result.ReasonString);
                }
            }
            finally
            {
                gate.Release();
            }
        }));
    }
}
