namespace BodBridge.Api.Domain;

// ============================================================================
// PLUG POINT (DB format): these are EXAMPLE entities with fictional fields.
// Replace InventoryItem with your real entities; keep BodRecord if you want
// an auditable copy of every BOD received (recommended). Entity shape is
// yours -- the mapper (Mapping/) is the only code that knows both sides.
// ============================================================================

// One row per BOD received, whatever its noun type. PayloadJson keeps the
// full noun so nothing is lost even before a typed mapper exists for it.
public sealed class BodRecord
{
    public Guid Id { get; set; } = Guid.NewGuid();
    public required string BodId { get; set; }
    public required string Verb { get; set; }
    public required string NounType { get; set; }
    public string? SenderId { get; set; }
    public DateTimeOffset? BodCreationDateTime { get; set; }
    // DateTime (not DateTimeOffset) on purpose: the SQLite provider cannot
    // ORDER BY DateTimeOffset. If you switch providers this can go back.
    public DateTime ReceivedAt { get; set; } = DateTime.UtcNow;
    public required string PayloadJson { get; set; }
}

// Example typed entity, produced from the generic sample BOD's noun.
public sealed class InventoryItem
{
    public Guid Id { get; set; } = Guid.NewGuid();
    public required string ExternalItemId { get; set; }   // <ItemID> in the BOD
    public string? Description { get; set; }
    public int QuantityOnHand { get; set; }
    public string? LocationCode { get; set; }
    public string? StatusCode { get; set; }

    public Guid BodRecordId { get; set; }
    public BodRecord? BodRecord { get; set; }
}
