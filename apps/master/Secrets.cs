using System.Buffers.Text;
using System.Security.Cryptography;
using System.Text;

namespace DigBlock.Master;

/// <summary>Opaque bearer secrets — session tokens, server registration
/// keys, and the one-time codes the sign-in redirect carries. CSPRNG bytes,
/// handed out base64url and stored as a bare SHA-256 — not passwords, so no
/// KDF; the hash is what keeps a database read (a backup, a support query, a
/// leak) from handing out a working credential.</summary>
public static class Secrets
{
    private const int Bytes = 32;

    public static string New() => Base64Url.EncodeToString(RandomNumberGenerator.GetBytes(Bytes));

    public static byte[] Hash(string secret) => SHA256.HashData(Encoding.UTF8.GetBytes(secret));

    /// <summary>Short, human-transcribable code for the sign-in page's
    /// address bar. Not a credential — it only names a pending request, and
    /// the actual authority is the one-time code plus the PKCE verifier.</summary>
    public static string NewHandle() => Base64Url.EncodeToString(RandomNumberGenerator.GetBytes(16));
}
