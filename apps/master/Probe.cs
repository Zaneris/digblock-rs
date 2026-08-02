using System.Net;
using System.Net.Quic;
using System.Net.Security;
using System.Net.Sockets;
using System.Security.Authentication;
using System.Security.Cryptography;

namespace DigBlock.Master;

public enum ProbeFailure
{
    Dns,
    Timeout,
    Handshake,
    CertMismatch,
}

public sealed record ProbeResult(bool Reachable, ProbeFailure? Reason = null, string? Detail = null)
{
    public string ReasonString => Reason switch
    {
        ProbeFailure.Dns => "dns",
        ProbeFailure.Timeout => "timeout",
        ProbeFailure.CertMismatch => "certMismatch",
        _ => "handshake",
    };
}

public interface IReachabilityProbe
{
    Task<ProbeResult> ProbeAsync(string host, int port, byte[] certHash, CancellationToken ct);
}

/// <summary>Connects to the game server the same way clients do at the
/// transport level: a QUIC + TLS 1.3 handshake with ALPN "h3", accepting the
/// certificate iff its SHA-256 matches the registered hash — the exact mirror
/// of the Rust client's PinnedSha256 policy. Handshake completion is the
/// success criterion; no HTTP/3 or WebTransport session is attempted.</summary>
// CA1416: only constructed behind the QuicConnection.IsSupported gate in
// Program.cs, which the platform-compatibility analyzer cannot see.
#pragma warning disable CA1416
public sealed class QuicProbe(TimeSpan timeout) : IReachabilityProbe
{
    public async Task<ProbeResult> ProbeAsync(string host, int port, byte[] certHash, CancellationToken ct)
    {
        if (!IPAddress.TryParse(host, out var address))
        {
            IPAddress[] addresses;
            try
            {
                addresses = await Dns.GetHostAddressesAsync(host, ct);
            }
            catch (Exception e)
            {
                return new ProbeResult(false, ProbeFailure.Dns, e.Message);
            }
            // The game server binds IPv4-only; an AAAA-first pick would
            // false-negative dual-stack hosts.
            address = addresses.FirstOrDefault(a => a.AddressFamily == AddressFamily.InterNetwork)
                ?? addresses.FirstOrDefault();
            if (address is null)
                return new ProbeResult(false, ProbeFailure.Dns, "hostname resolved to no addresses");
        }

        var certMismatch = false;
        var options = new QuicClientConnectionOptions
        {
            RemoteEndPoint = new IPEndPoint(address, port),
            DefaultStreamErrorCode = 0x0100,
            DefaultCloseErrorCode = 0x0100,
            ClientAuthenticationOptions = new SslClientAuthenticationOptions
            {
                ApplicationProtocols = [new SslApplicationProtocol("h3")],
                TargetHost = host,
                RemoteCertificateValidationCallback = (_, cert, _, _) =>
                {
                    var ok = cert is not null
                        && CryptographicOperations.FixedTimeEquals(
                            cert.GetCertHash(HashAlgorithmName.SHA256), certHash);
                    certMismatch = !ok;
                    return ok;
                },
            },
        };

        using var cts = CancellationTokenSource.CreateLinkedTokenSource(ct);
        cts.CancelAfter(timeout);
        try
        {
            var connection = await QuicConnection.ConnectAsync(options, cts.Token);
            await connection.DisposeAsync();
            return new ProbeResult(true);
        }
        catch (OperationCanceledException) when (!ct.IsCancellationRequested)
        {
            return new ProbeResult(false, ProbeFailure.Timeout, $"no QUIC handshake within {timeout.TotalSeconds:0}s");
        }
        catch (AuthenticationException e)
        {
            return new ProbeResult(false, certMismatch ? ProbeFailure.CertMismatch : ProbeFailure.Handshake, e.Message);
        }
        catch (Exception e)
        {
            return new ProbeResult(false, certMismatch ? ProbeFailure.CertMismatch : ProbeFailure.Handshake, e.Message);
        }
    }
}
#pragma warning restore CA1416
