using System.Data.Common;
using Microsoft.EntityFrameworkCore.Diagnostics;

namespace DigBlock.Master.Data;

/// <summary>Forces SQLite's rollback journal back to DELETE on every connection, AFTER EF Core's
/// provider issues <c>PRAGMA journal_mode = 'wal'</c> on open, so this one sticks. WAL needs a
/// shared-memory <c>-shm</c> file and is unsupported over the network filesystem the production
/// volume is (Azure Files is SMB), so left alone the master works in development and corrupts in
/// production. Unconditional on purpose: a mode that differs is a mode nobody tests.</summary>
public sealed class JournalModeInterceptor : DbConnectionInterceptor
{
    public override void ConnectionOpened(DbConnection connection, ConnectionEndEventData eventData)
        => Apply(connection);

    public override async Task ConnectionOpenedAsync(
        DbConnection connection, ConnectionEndEventData eventData, CancellationToken ct = default)
    {
        await using var cmd = connection.CreateCommand();
        cmd.CommandText = "PRAGMA journal_mode=DELETE;";
        await cmd.ExecuteNonQueryAsync(ct);
    }

    private static void Apply(DbConnection connection)
    {
        using var cmd = connection.CreateCommand();
        cmd.CommandText = "PRAGMA journal_mode=DELETE;";
        cmd.ExecuteNonQuery();
    }
}
