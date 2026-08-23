using System.Net;

namespace DigBlock.Master;

/// <summary>Which peers may set <c>X-Forwarded-For</c> and be believed. The source address is the
/// probe's first dial, each rate limit's partition and a temp-ban's subject, so an open header
/// lets a caller serve someone else's ban or pick a fresh bucket. <c>ForwardLimit = 1</c> does not
/// close that — only a known-proxy list says whether the peer was an ingress. Believe it only from
/// a peer <see cref="ProbeTargets.IsPublic"/> rejects, and keep the two in step.</summary>
public static class TrustedProxies
{
    /// <summary>Ranges that never appear as a public-internet caller, so a peer inside one is
    /// infrastructure in front of us rather than a client; kept in step with
    /// <see cref="ProbeTargets.IsPublic"/>. Residual and accepted: on a LAN or a Docker network a
    /// peer already inside can still spoof the header — the same trust boundary
    /// <c>Master:Probe:AllowPrivateTargets</c> draws.</summary>
    private static readonly (string Prefix, int Bits)[] BuiltIn =
    [
        ("0.0.0.0", 8),        // "this network"
        ("10.0.0.0", 8),       // RFC 1918 private
        ("100.64.0.0", 10),    // carrier NAT — Azure Container Apps ingress
        ("127.0.0.0", 8),      // loopback
        ("169.254.0.0", 16),   // link-local
        ("172.16.0.0", 12),    // RFC 1918 private
        ("192.168.0.0", 16),   // RFC 1918 private
        ("198.18.0.0", 15),    // benchmarking
        ("::1", 128),          // loopback
        ("fc00::", 7),         // unique-local
        ("fe80::", 10),        // link-local
    ];

    /// <summary>The built-in set plus any operator-configured extras.
    /// A malformed CIDR is fatal rather than skipped: a proxy network that
    /// silently did not parse is a master that attributes every registration
    /// to its own ingress, which looks like a networking fault and not a
    /// typo.</summary>
    public static List<IPNetwork> Networks(IEnumerable<string> extra)
    {
        var nets = new List<IPNetwork>();
        foreach (var (prefix, bits) in BuiltIn)
            nets.Add(new IPNetwork(IPAddress.Parse(prefix), bits));

        foreach (var cidr in extra)
        {
            if (string.IsNullOrWhiteSpace(cidr))
                continue;
            if (!IPNetwork.TryParse(cidr.Trim(), out var net))
                throw new FormatException(
                    $"Master:Net:TrustedProxyNetworks contains \"{cidr}\", which is not a CIDR range (e.g. 203.0.113.0/24)");
            nets.Add(net);
        }
        return nets;
    }
}
