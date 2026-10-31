using BodBridge.Api.Contracts;
using BodBridge.Api.Data;
using BodBridge.Api.Domain;
using BodBridge.Api.Mapping;
using Microsoft.EntityFrameworkCore;

var builder = WebApplication.CreateBuilder(args);

// --- Database (PLUG POINT: provider + connection string) -------------------
builder.Services.AddDbContext<AppDbContext>(options =>
    options.UseSqlite(builder.Configuration.GetConnectionString("Default")
                      ?? "Data Source=bodbridge.db"));
// SQL Server example:  options.UseSqlServer(builder.Configuration.GetConnectionString("Default"))
// PostgreSQL example: options.UseNpgsql(builder.Configuration.GetConnectionString("Default"))

// --- Mappers (PLUG POINT: one IBodMapper per noun type) ---------------------
builder.Services.AddScoped<IBodMapper, InventoryItemMapper>();

var app = builder.Build();

// Prototype convenience: create the schema if missing.
// Real deployment: use EF Core migrations instead and delete these two lines.
using (var scope = app.Services.CreateScope())
    scope.ServiceProvider.GetRequiredService<AppDbContext>().Database.EnsureCreated();

// HTTPS only in the sample run script (Kestrel endpoint + cert come from
// environment/config there). In production, terminate TLS here with a real
// certificate or behind a TLS-terminating gateway -- do not add UseHttpRedirection
// shortcuts that silently fall back to plain HTTP for machine-to-machine traffic.

// Receive a BOD transport DTO, store it, and map it to typed EF entities.
app.MapPost("/api/bods", async (BodTransportDto dto, AppDbContext db, IEnumerable<IBodMapper> mappers) =>
{
    if (await db.BodRecords.AnyAsync(x => x.BodId == dto.BodId))
        return Results.Conflict(new { error = $"BOD {dto.BodId} already stored" });

    var record = new BodRecord
    {
        BodId = dto.BodId,
        Verb = dto.Verb,
        NounType = dto.NounType,
        SenderId = dto.SenderId,
        BodCreationDateTime = dto.CreationDateTime,
        PayloadJson = dto.Payload.GetRawText(),
    };
    db.BodRecords.Add(record);

    object? entity = mappers.FirstOrDefault(m => m.NounType == dto.NounType)
                            ?.TryMap(dto, record);
    if (entity is not null) db.Add(entity);
    await db.SaveChangesAsync();

    return Results.Created($"/api/bods/{record.Id}",
        new { record.Id, record.BodId, record.NounType, mapped = entity?.GetType().Name });
});

// The "C# program that reads the EF Core objects" side of the pattern:
// plain DbContext queries, no XML, no HTTP DTOs.
app.MapGet("/api/bods", async (AppDbContext db) =>
    await db.BodRecords.OrderByDescending(x => x.ReceivedAt)
            .Select(x => new { x.Id, x.BodId, x.Verb, x.NounType, x.SenderId, x.ReceivedAt })
            .ToListAsync());

app.MapGet("/api/items", async (AppDbContext db) =>
    await db.InventoryItems.OrderBy(x => x.ExternalItemId)
            .Select(x => new { x.ExternalItemId, x.Description, x.QuantityOnHand, x.LocationCode, x.StatusCode })
            .ToListAsync());

app.MapGet("/health", () => Results.Ok(new { status = "ok" }));

app.Run();
