# BodBridge — generic C++ → HTTPS → C# / EF Core pattern

A working, end-to-end reference for this integration pattern:

```
XML BOD file ──► C++ bod-sender ──HTTPS (JSON)──► C# ASP.NET Core API
                                                      │  IBodMapper
                                                      ▼
                                              EF Core entities ──► database
                                                      │
                              any other C# program ◄──┘  (plain DbContext queries)
```

Everything in here is **generic and fictional** — the sample noun is an
`InventoryItem` widget, the database is a throwaway SQLite file, the cert is
a throwaway self-signed localhost cert. The proprietary pieces (your real
BOD nouns/XSDs and your real DB schema) plug in at four marked seams without
touching the transport code. Search the code for `PLUG POINT`.

## One honest design note

EF Core objects do **not** cross the wire. An EF entity only exists inside a
C# process, tracked by a `DbContext` — C++ cannot send one, and neither can
another C# process. What crosses HTTPS is a **transport DTO** (JSON, schema
in `contracts/bod-transport.schema.json`) that mirrors the BOD. The C#
service maps DTO → EF entities on arrival. This is not a workaround, it is
the standard shape of this pattern, and it is what makes the proprietary
parts pluggable: wire format, mapping, and DB schema evolve independently.

## Run the demo

Requirements: a C++17 compiler + CMake + libcurl + libxml2 dev packages,
and the .NET 8 SDK.

```bash
./scripts/run-e2e.sh
```

That builds both sides, starts the API on `https://localhost:8443` with a
self-signed cert, sends `sample/sample-bod.xml` with the C++ program
(verifying TLS against that cert), and reads the stored EF entities back.
Expected tail of the output:

```
== C++ sender ==
HTTP 201
{...,"bodId":"example-bod-0001","nounType":"InventoryItem","mapped":"InventoryItem"}
== GET /api/items (typed EF entities) ==
[{"externalItemId":"ITEM-1001","description":"Generic widget, blue","quantityOnHand":42,...}]
E2E OK
```

Re-sending the same BODID returns **HTTP 409** (BodRecord.BodId has a unique
index) — idempotency behaviour is yours to keep or change in `Program.cs`.

Run the pieces separately:

```bash
# C++
cmake -S cpp -B cpp/build && cmake --build cpp/build
./cpp/build/bod-sender sample/sample-bod.xml --print-json   # see the DTO, no server needed

# C#  (run it from its project directory — ASP.NET Core takes its content
#      root from the working directory)
cd csharp/BodBridge.Api && dotnet run -c Release
```

## The four plug points

| # | Seam | Where | What you replace |
|---|------|-------|------------------|
| 1 | XML format | `cpp/src/BodParser.h` — `IBodParser` | The generic libxml2 DOM walk. Swap in an XSD-generated parser, a streaming parser, signature checks, etc. Downstream only sees the generic `XmlNode` tree. |
| 2 | Wire mapping (C++) | `cpp/src/TransportMapper.h` — `ITransportMapper` | The generic noun→JSON mirror. Swap in a hand-shaped typed DTO if your wire contract should not mirror the XML. |
| 3 | Noun mapping (C#) | `csharp/BodBridge.Api/Mapping/IBodMapper.cs` | One mapper per real noun type: DTO → your EF entities. Unmapped nouns are still stored verbatim as `BodRecord`s, so nothing is dropped while mappers are being written. Register each mapper in `Program.cs`. |
| 4 | DB format | `csharp/BodBridge.Api/Domain/Entities.cs`, `Data/AppDbContext.cs`, provider in `Program.cs`, connection string in `appsettings.json` | Your real entities/`DbContext`. Swap the EF provider package (`Sqlite` → `SqlServer` / `Npgsql`) and the `UseXxx` call; entities and mappers do not change. For production, replace `EnsureCreated()` in `Program.cs` with EF Core migrations. |

Auth/TLS plug points, same idea: the C++ client already supports a CA bundle,
an mTLS client cert/key (`--cert` / `--key`), and an API-key header
(`--api-key` / `BODBRIDGE_API_KEY`) — replace the header with your scheme in
`cpp/src/HttpsClient.cpp` and enforce it in the API. TLS verification is on
by default; `--insecure-localhost-only` exists for local dev and the client
refuses to combine it with a non-localhost URL.

## Layout

```
contracts/    bod-transport.schema.json — the wire contract, the only shared truth
sample/       sample-bod.xml — fictional ProcessInventoryItem BOD
cpp/          bod-sender (plug points 1–2)
csharp/       BodBridge.Api (plug points 3–4): Contracts/ Domain/ Data/ Mapping/
scripts/      run-e2e.sh — builds, serves, sends, reads back, asserts nothing silently
```

## Verified in this repo's test run (2026-10-31)

- C++ builds clean under `-Wall -Wextra`; C# builds 0 warnings / 0 errors.
- Full HTTPS round trip with certificate verification (no `-k` shortcuts):
  POST → 201 with the noun mapped to a typed EF entity; both read-back
  endpoints return the stored data.
- Duplicate BODID → 409.
- One real bug was found and fixed by the e2e run: SQLite cannot
  `ORDER BY DateTimeOffset`, so `BodRecord.ReceivedAt` is a UTC `DateTime`.
  If you switch providers, that constraint disappears — the comment is in
  `Domain/Entities.cs`.
