using System;
using Microsoft.EntityFrameworkCore.Migrations;

#nullable disable

namespace DigBlock.Master.Data.Migrations
{
    /// <inheritdoc />
    public partial class InitialAccounts : Migration
    {
        /// <inheritdoc />
        protected override void Up(MigrationBuilder migrationBuilder)
        {
            migrationBuilder.CreateTable(
                name: "Accounts",
                columns: table => new
                {
                    Id = table.Column<Guid>(type: "TEXT", nullable: false),
                    CreatedUtc = table.Column<DateTimeOffset>(type: "TEXT", nullable: false),
                    LastSeenUtc = table.Column<DateTimeOffset>(type: "TEXT", nullable: false),
                    Status = table.Column<int>(type: "INTEGER", nullable: false),
                    DisplayName = table.Column<string>(type: "TEXT", maxLength: 10, nullable: true),
                    DisplayNameNormalized = table.Column<string>(type: "TEXT", maxLength: 10, nullable: true),
                    DisplayNameChangedUtc = table.Column<DateTimeOffset>(type: "TEXT", nullable: true),
                    BanUntilUtc = table.Column<DateTimeOffset>(type: "TEXT", nullable: true),
                    BanReason = table.Column<string>(type: "TEXT", maxLength: 200, nullable: true)
                },
                constraints: table =>
                {
                    table.PrimaryKey("PK_Accounts", x => x.Id);
                });

            migrationBuilder.CreateTable(
                name: "NameReservations",
                columns: table => new
                {
                    NameNormalized = table.Column<string>(type: "TEXT", maxLength: 10, nullable: false),
                    AccountId = table.Column<Guid>(type: "TEXT", nullable: false),
                    ReleasedUtc = table.Column<DateTimeOffset>(type: "TEXT", nullable: false)
                },
                constraints: table =>
                {
                    table.PrimaryKey("PK_NameReservations", x => x.NameNormalized);
                });

            migrationBuilder.CreateTable(
                name: "ServerClaims",
                columns: table => new
                {
                    Id = table.Column<Guid>(type: "TEXT", nullable: false),
                    Host = table.Column<string>(type: "TEXT", maxLength: 255, nullable: false),
                    Port = table.Column<int>(type: "INTEGER", nullable: false),
                    KeyHash = table.Column<byte[]>(type: "BLOB", maxLength: 32, nullable: false),
                    Name = table.Column<string>(type: "TEXT", maxLength: 24, nullable: false),
                    FirstSeenUtc = table.Column<DateTimeOffset>(type: "TEXT", nullable: false),
                    LastRegisteredUtc = table.Column<DateTimeOffset>(type: "TEXT", nullable: false)
                },
                constraints: table =>
                {
                    table.PrimaryKey("PK_ServerClaims", x => x.Id);
                });

            migrationBuilder.CreateTable(
                name: "ProviderLinks",
                columns: table => new
                {
                    Id = table.Column<long>(type: "INTEGER", nullable: false)
                        .Annotation("Sqlite:Autoincrement", true),
                    AccountId = table.Column<Guid>(type: "TEXT", nullable: false),
                    Provider = table.Column<int>(type: "INTEGER", nullable: false),
                    Subject = table.Column<string>(type: "TEXT", maxLength: 128, nullable: false),
                    LinkedUtc = table.Column<DateTimeOffset>(type: "TEXT", nullable: false)
                },
                constraints: table =>
                {
                    table.PrimaryKey("PK_ProviderLinks", x => x.Id);
                    table.ForeignKey(
                        name: "FK_ProviderLinks_Accounts_AccountId",
                        column: x => x.AccountId,
                        principalTable: "Accounts",
                        principalColumn: "Id",
                        onDelete: ReferentialAction.Cascade);
                });

            migrationBuilder.CreateTable(
                name: "Sessions",
                columns: table => new
                {
                    Id = table.Column<Guid>(type: "TEXT", nullable: false),
                    AccountId = table.Column<Guid>(type: "TEXT", nullable: false),
                    TokenHash = table.Column<byte[]>(type: "BLOB", maxLength: 32, nullable: false),
                    ChainId = table.Column<Guid>(type: "TEXT", nullable: false),
                    CreatedUtc = table.Column<DateTimeOffset>(type: "TEXT", nullable: false),
                    LastSeenUtc = table.Column<DateTimeOffset>(type: "TEXT", nullable: false),
                    ExpiresUtc = table.Column<DateTimeOffset>(type: "TEXT", nullable: false),
                    RevokedUtc = table.Column<DateTimeOffset>(type: "TEXT", nullable: true),
                    RevokedReason = table.Column<int>(type: "INTEGER", nullable: true),
                    Platform = table.Column<string>(type: "TEXT", maxLength: 16, nullable: true)
                },
                constraints: table =>
                {
                    table.PrimaryKey("PK_Sessions", x => x.Id);
                    table.ForeignKey(
                        name: "FK_Sessions_Accounts_AccountId",
                        column: x => x.AccountId,
                        principalTable: "Accounts",
                        principalColumn: "Id",
                        onDelete: ReferentialAction.Cascade);
                });

            migrationBuilder.CreateIndex(
                name: "IX_Accounts_DisplayNameNormalized",
                table: "Accounts",
                column: "DisplayNameNormalized",
                unique: true);

            migrationBuilder.CreateIndex(
                name: "IX_NameReservations_ReleasedUtc",
                table: "NameReservations",
                column: "ReleasedUtc");

            migrationBuilder.CreateIndex(
                name: "IX_ProviderLinks_AccountId",
                table: "ProviderLinks",
                column: "AccountId");

            migrationBuilder.CreateIndex(
                name: "IX_ProviderLinks_Provider_Subject",
                table: "ProviderLinks",
                columns: new[] { "Provider", "Subject" },
                unique: true);

            migrationBuilder.CreateIndex(
                name: "IX_ServerClaims_Host_Port",
                table: "ServerClaims",
                columns: new[] { "Host", "Port" },
                unique: true);

            migrationBuilder.CreateIndex(
                name: "IX_ServerClaims_KeyHash",
                table: "ServerClaims",
                column: "KeyHash",
                unique: true);

            migrationBuilder.CreateIndex(
                name: "IX_Sessions_AccountId",
                table: "Sessions",
                column: "AccountId");

            migrationBuilder.CreateIndex(
                name: "IX_Sessions_ChainId",
                table: "Sessions",
                column: "ChainId");

            migrationBuilder.CreateIndex(
                name: "IX_Sessions_ExpiresUtc",
                table: "Sessions",
                column: "ExpiresUtc");

            migrationBuilder.CreateIndex(
                name: "IX_Sessions_TokenHash",
                table: "Sessions",
                column: "TokenHash",
                unique: true);
        }

        /// <inheritdoc />
        protected override void Down(MigrationBuilder migrationBuilder)
        {
            migrationBuilder.DropTable(
                name: "NameReservations");

            migrationBuilder.DropTable(
                name: "ProviderLinks");

            migrationBuilder.DropTable(
                name: "ServerClaims");

            migrationBuilder.DropTable(
                name: "Sessions");

            migrationBuilder.DropTable(
                name: "Accounts");
        }
    }
}
