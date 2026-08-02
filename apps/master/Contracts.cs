namespace DigBlock.Master;

// Wire shapes for the master API. Serialized camelCase (ASP.NET web default).
// This contract is mirrored by the Rust side in apps/server/src/master.rs and
// apps/desktop/src/services.rs — keep them in lockstep.

public sealed record RegisterRequest(
    string? Name,
    string? Host, // optional; defaults to the observed remote IP
    int Port,
    string? CertHash, // 64 hex chars (SHA-256 of the server's TLS cert)
    int Players,
    int MaxPlayers,
    int ProtocolVersion,
    int SimVersion,
    // Optional alternate address for reachability probes (initial and
    // periodic) when the master's network view differs from clients' —
    // e.g. a containerized master probing host.docker.internal while
    // clients dial 127.0.0.1. Clients always get Host; production
    // servers omit this.
    string? ProbeHost = null);

public sealed record RegisterOk(bool Reachable, Guid ServerId, string ServerKey, string ObservedHost);

public sealed record RegisterFailed(bool Reachable, string Reason, string Detail);

public sealed record HeartbeatRequest(int Players, int MaxPlayers);

public sealed record ServerSummary(Guid Id, string Name, string Url, string CertHash, int Players, int MaxPlayers);

public sealed record ServerListResponse(IReadOnlyList<ServerSummary> Servers);

public sealed record JoinRequest(Guid ServerId, string? DisplayName);

public sealed record JoinResponse(string Token, int ExpiresIn, string Url, string CertHash);

public sealed record VerifyRequest(string? Token);

public sealed record VerifyResponse(string DisplayName);
