using System.Net;
using System.Net.Sockets;

namespace DigBlock.Master;

/// <summary>Which addresses the reachability probe is allowed to dial.
///
/// Registration is anonymous, so <c>/api/servers/register</c> is an
/// unauthenticated way to make the master open a connection to an address of
/// the caller's choosing and report what happened — an SSRF probe with a
/// four-value error oracle (NET-AUDIT-2.md §1.7). The oracle itself is worth
/// keeping: a self-hoster genuinely needs to know <c>certMismatch</c> from
/// <c>timeout</c> to debug a port forward. What is NOT worth keeping is the
/// set of interesting targets, and that is what this removes — everything
/// off the public internet, which is where a hosted master's neighbours
/// live.
///
/// The check has to happen after DNS resolution, not on the string: a
/// hostname that resolves to 10.0.0.5 is the whole trick.</summary>
public static class ProbeTargets
{
    /// <summary>Is this an address on the public internet?</summary>
    public static bool IsPublic(IPAddress address)
    {
        if (address.IsIPv4MappedToIPv6)
            address = address.MapToIPv4();

        if (IPAddress.IsLoopback(address)
            || address.Equals(IPAddress.Any)
            || address.Equals(IPAddress.IPv6Any)
            || address.Equals(IPAddress.Broadcast))
            return false;

        if (address.AddressFamily == AddressFamily.InterNetworkV6)
        {
            // Link-local (fe80::/10), site-local (fec0::/10, deprecated),
            // multicast, and unique-local (fc00::/7).
            if (address.IsIPv6LinkLocal || address.IsIPv6SiteLocal || address.IsIPv6Multicast)
                return false;
            var v6 = address.GetAddressBytes();
            return (v6[0] & 0xFE) != 0xFC;
        }

        var b = address.GetAddressBytes();
        return b[0] switch
        {
            0 => false,                          // 0.0.0.0/8 "this network"
            10 => false,                         // private
            127 => false,                        // loopback
            100 => (b[1] & 0xC0) != 64,          // 100.64.0.0/10 carrier NAT
            169 => b[1] != 254,                  // 169.254.0.0/16 link-local
            172 => (b[1] & 0xF0) != 16,          // 172.16.0.0/12 private
            192 => !(b[1] == 168                 // 192.168.0.0/16 private
                     || (b[1] == 0 && b[2] == 0) // 192.0.0.0/24 IETF protocol
                     || (b[1] == 0 && b[2] == 2) // 192.0.2.0/24 TEST-NET-1
                     || (b[1] == 88 && b[2] == 99)), // 192.88.99.0/24 6to4 relay
            198 => !((b[1] & 0xFE) == 18         // 198.18.0.0/15 benchmarking
                     || (b[1] == 51 && b[2] == 100)), // 198.51.100.0/24 TEST-NET-2
            203 => !(b[1] == 0 && b[2] == 113),  // 203.0.113.0/24 TEST-NET-3
            >= 224 => false,                     // multicast and reserved
            _ => true,
        };
    }
}
