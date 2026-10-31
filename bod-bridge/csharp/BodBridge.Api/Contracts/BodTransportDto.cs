using System.Text.Json;

namespace BodBridge.Api.Contracts;

// The ONLY type that crosses the HTTPS wire (see contracts/bod-transport.schema.json).
// It mirrors the BOD: header fields lifted out, Payload = the noun as generic JSON.
// EF Core entities live in Domain/ and are created from this DTO by an IBodMapper.
public sealed class BodTransportDto
{
    public required string BodId { get; init; }
    public required string Verb { get; init; }
    public required string NounType { get; init; }
    public string? SenderId { get; init; }
    public DateTimeOffset? CreationDateTime { get; init; }
    public required JsonElement Payload { get; init; }
}
