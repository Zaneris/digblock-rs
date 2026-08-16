using System.Net;

namespace DigBlock.Master;

/// <summary>Which peers may set <c>X-Forwarded-For</c> and be believed.
///
/// The observed source address used to be cosmetic — the default host for a
/// server that registered without one — so the forwarded-headers middleware
/// ran with its known-proxy lists CLEARED, trusting the header from anybody.
/// That is no longer safe: the source address is now the first address the
/// registration probe dials, the partition every rate limit is cut by, and
/// the subject of a registration temp-ban. Left open, a direct caller could
/// send <c>X-Forwarded-For: 1.2.3.4</c> to serve someone else's ban, evade
/// their own, or pick a fresh rate-limit bucket per request.
///
/// <c>ForwardLimit = 1</c> does NOT close that. It bounds how many hops are
/// walked, which stops a client PREPENDING entries behind a real ingress; it
/// says nothing about whether the immediate peer was an ingress at all.
/// Only a known-proxy list answers that.
///
/// The rule: believe the header only from a peer whose own address cannot
/// appear as a caller on the public internet — an ingress, a reverse proxy,
/// a Docker bridge. That is the same partition <see cref="ProbeTargets"/>
/// draws for SSRF, from the other side, and the two lists must keep saying
/// the same thing: every network here is one <see cref="ProbeTargets.IsPublic"/>
/// rejects.
///
/// Azure Container Apps is the reason the loopback-only framework default
/// will not do — its ingress reaches the app from the platform's own
/// 100.64.0.0/10 space, which is carrier-NAT and unroutable from outside.
///
/// Residual, and accepted: on a LAN or a Docker network, a peer that is
/// already inside can still spoof the header. That is the same trust
/// boundary <c>Master:Probe:AllowPrivateTargets</c> draws — an operator's
/// own network is theirs.</summary>
public static class TrustedProxies
{
    /// <summary>Ranges that never appear as a public-internet caller, so a
    /// peer inside one is infrastructure in front of us rather than a
    /// client. Kept in step with <see cref="ProbeTargets.IsPublic"/>.</summary>
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
