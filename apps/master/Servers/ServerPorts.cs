namespace DigBlock.Master.Servers;

/// <summary>The UDP ports a game server may be listed at. A block rather than
/// the whole port space, for two reasons that both point outward: registering
/// makes the master dial <c>host:port</c> with a QUIC handshake, and listing
/// aims every client at it — so an anonymous caller must not get to pick an
/// arbitrary port on a host it names. The block also gives an operator running
/// several servers on one machine one firewall rule. Mirrored by
/// <c>PORT_RANGE</c> in the game server (<c>apps/server/src/lib.rs</c>), which
/// refuses to boot with an advertised port outside it and whose test reads
/// this file — change both together.</summary>
public static class ServerPorts
{
    public const int Min = 4433;
    public const int Max = 4499;

    public static bool Allows(int port) => port is >= Min and <= Max;
}
