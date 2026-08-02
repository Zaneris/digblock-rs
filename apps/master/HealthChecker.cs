using Microsoft.Extensions.Options;

namespace DigBlock.Master;

/// <summary>Periodically re-probes registered servers (skipping ones that
/// heartbeated within the interval) and removes them after MaxStrikes
/// consecutive failures. Also sweeps expired join tokens.</summary>
public sealed class HealthChecker(
    ServerRegistry registry,
    TokenStore tokens,
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

        var maxStrikes = options.Value.Probe.MaxStrikes;
        var now = DateTimeOffset.UtcNow;

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
