using System.Data.Common;
using Microsoft.EntityFrameworkCore.Diagnostics;

namespace DigBlock.Master.Data;

/// <summary>Forces SQLite's rollback journal back to DELETE on every
/// connection.
///
/// EF Core's SQLite provider issues <c>PRAGMA journal_mode = 'wal'</c> itself
/// on open — a good default on a local disk and a broken one on the
/// filesystem this is deployed to. WAL needs a shared-memory <c>-shm</c>
/// file and SQLite documents it as unsupported over a network filesystem;
/// Azure Files is SMB. Left alone, the master would work perfectly in
/// development and corrupt or lock up in production, which is the worst
/// possible failure shape.
///
/// The pragma runs AFTER the provider's, so this one is the one that
/// sticks. It is unconditional rather than configurable on purpose: a mode
/// that differs between a developer's machine and production is a mode
/// nobody tests.</summary>
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
