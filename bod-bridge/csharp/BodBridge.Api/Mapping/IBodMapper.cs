using System.Text.Json;
using BodBridge.Api.Contracts;
using BodBridge.Api.Domain;

namespace BodBridge.Api.Mapping;

// ============================================================================
// PLUG POINT (noun mapping, C# side): IBodMapper
// TryMap returns a typed EF entity for a noun type this mapper understands,
// or null if it does not (the BOD is still stored as a BodRecord -- nothing
// is ever dropped). Add one mapper per real noun type and register it in
// Program.cs; the endpoint code does not change.
// ============================================================================
public interface IBodMapper
{
    string NounType { get; }
    object? TryMap(BodTransportDto dto, BodRecord record);
}

public sealed class InventoryItemMapper : IBodMapper
{
    public string NounType => "InventoryItem";

    public object? TryMap(BodTransportDto dto, BodRecord record)
    {
        JsonElement p = dto.Payload;
        string? Get(string name) =>
            p.TryGetProperty(name, out JsonElement v) && v.ValueKind == JsonValueKind.String
                ? v.GetString()
                : null;

        string? itemId = Get("ItemID");
        if (itemId is null) return null;   // payload does not match the expected shape

        int.TryParse(Get("QuantityOnHand"), out int qty);
        return new InventoryItem
        {
            ExternalItemId = itemId,
            Description = Get("Description"),
            QuantityOnHand = qty,
            LocationCode = Get("LocationCode"),
            StatusCode = Get("StatusCode"),
            BodRecordId = record.Id,
            BodRecord = record,
        };
    }
}
