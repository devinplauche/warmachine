using BodBridge.Api.Domain;
using Microsoft.EntityFrameworkCore;

namespace BodBridge.Api.Data;

// ============================================================================
// PLUG POINT (DB format): your real DbContext replaces/extends this one.
// Provider is chosen in Program.cs; connection string in appsettings.json.
// For a real deployment replace EnsureCreated (Program.cs) with migrations:
//   dotnet ef migrations add Initial && dotnet ef database update
// ============================================================================
public sealed class AppDbContext(DbContextOptions<AppDbContext> options) : DbContext(options)
{
    public DbSet<BodRecord> BodRecords => Set<BodRecord>();
    public DbSet<InventoryItem> InventoryItems => Set<InventoryItem>();

    protected override void OnModelCreating(ModelBuilder b)
    {
        b.Entity<BodRecord>(e =>
        {
            e.HasIndex(x => x.BodId).IsUnique();   // re-sending the same BODID is rejected
            e.Property(x => x.BodId).HasMaxLength(128);
        });
        b.Entity<InventoryItem>(e =>
        {
            e.HasIndex(x => x.ExternalItemId);
            e.HasOne(x => x.BodRecord).WithMany()
             .HasForeignKey(x => x.BodRecordId).OnDelete(DeleteBehavior.Cascade);
        });
    }
}
