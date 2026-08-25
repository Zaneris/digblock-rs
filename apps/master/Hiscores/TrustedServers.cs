using DigBlock.Master.Servers;
using Microsoft.Extensions.Options;

namespace DigBlock.Master.Hiscores;

/// <summary>The trusted list as a predicate over a registration. Matched on the address a
/// server is LISTED at — which the probe verified — never on anything the request body says,
/// so being trusted needs the machine at that address, not knowledge of its name.</summary>
public sealed class TrustedServers(IOptions<MasterOptions> options)
{
    public bool IsTrusted(ServerEntry entry) => IsTrusted(entry.Host, entry.Port);

    public bool IsTrusted(string host, int port)
    {
        var norm = ServerClaims.Normalize(host);
        foreach (var raw in options.Value.Hiscores.TrustedServers)
        {
            var listed = raw.Trim().ToLowerInvariant();
            if (listed.Length == 0)
                continue;
            // An IPv6 literal with a port is bracketed in config the way a URL writes it.
            if (listed == norm || listed == $"{norm}:{port}" || listed == $"[{norm}]:{port}")
                return true;
        }
        return false;
    }
}
