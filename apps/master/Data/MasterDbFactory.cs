using Microsoft.EntityFrameworkCore;
using Microsoft.EntityFrameworkCore.Design;

namespace DigBlock.Master.Data;

/// <summary>Lets <c>dotnet ef migrations</c> build the model without booting
/// the web app.
///
/// Without this, EF falls back to running Program.cs to find the context —
/// which trips the mandatory QUIC-support gate, so authoring a migration
/// would require libmsquic on the developer's machine for no reason. The
/// path here is design-time scaffolding only; generated migrations describe
/// a schema, not a file.</summary>
public sealed class MasterDbFactory : IDesignTimeDbContextFactory<MasterDb>
{
    public MasterDb CreateDbContext(string[] args) =>
        new(new DbContextOptionsBuilder<MasterDb>().UseSqlite("Data Source=design-time.db").Options);
}
