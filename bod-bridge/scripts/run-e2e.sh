#!/usr/bin/env bash
# End-to-end demo: C++ bod-sender --HTTPS--> C# BodBridge.Api --EF Core--> SQLite
# Uses a throwaway self-signed cert for localhost and a throwaway database.
# Production: replace the cert with a real one (or your org CA) and the
# SQLite file with your real database -- no code changes.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$ROOT/.e2e"
mkdir -p "$WORK"
export TMPDIR="${TMPDIR:-$WORK/tmp}"; mkdir -p "$TMPDIR"
export DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_NOLOGO=1
DOTNET="${DOTNET:-dotnet}"
command -v "$DOTNET" >/dev/null || DOTNET="$HOME/workspace/tools/dotnet/dotnet"

# 1. Self-signed localhost cert (server) -- the C++ client verifies against it.
if [ ! -f "$WORK/server.pem" ]; then
  openssl req -x509 -newkey rsa:2048 -nodes \
    -keyout "$WORK/server.key" -out "$WORK/server.pem" -days 2 \
    -subj "/CN=localhost" -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" >/dev/null 2>&1
  # Kestrel wants a PKCS#12 bundle.
  openssl pkcs12 -export -inkey "$WORK/server.key" -in "$WORK/server.pem" \
    -out "$WORK/server.pfx" -passout pass:dev-only >/dev/null 2>&1
fi

# 2. Build both sides.
cmake -S "$ROOT/cpp" -B "$ROOT/cpp/build" -DCMAKE_BUILD_TYPE=Release >/dev/null
cmake --build "$ROOT/cpp/build" -j4 >/dev/null
"$DOTNET" build "$ROOT/csharp/BodBridge.Api" -c Release >/dev/null

# 3. Start the API on HTTPS :8443 with its own throwaway DB.
rm -f "$WORK/bodbridge.db"
API_DLL="$ROOT/csharp/BodBridge.Api/bin/Release/net8.0/BodBridge.Api.dll"
# Run the API with its working directory set to $WORK: ASP.NET Core uses the
# working directory as its content root, and starting it from a large home
# directory can stall startup. In normal use you run it from its project dir.
(
  cd "$WORK"
  ASPNETCORE_URLS="https://localhost:8443" \
  ASPNETCORE_Kestrel__Certificates__Default__Path="$WORK/server.pfx" \
  ASPNETCORE_Kestrel__Certificates__Default__Password="dev-only" \
  ConnectionStrings__Default="Data Source=$WORK/bodbridge.db" \
    exec "$DOTNET" "$API_DLL" > "$WORK/api.log" 2>&1
) &
API_PID=$!
trap 'kill $API_PID 2>/dev/null || true' EXIT

HEALTHY=0
for i in $(seq 1 30); do
  if curl -s --cacert "$WORK/server.pem" https://localhost:8443/health >/dev/null; then HEALTHY=1; break; fi
  sleep 1
done
if [ "$HEALTHY" != "1" ]; then
  echo "API did not become healthy; log follows:" >&2
  cat "$WORK/api.log" >&2
  exit 1
fi
echo "== health =="
curl -s --cacert "$WORK/server.pem" https://localhost:8443/health; echo

# 4. C++: parse the sample BOD, POST it over verified HTTPS.
echo "== C++ sender =="
"$ROOT/cpp/build/bod-sender" "$ROOT/sample/sample-bod.xml" \
  --url https://localhost:8443/api/bods --ca "$WORK/server.pem"

# 5. Read the EF Core objects back through the C# API.
echo "== GET /api/items (typed EF entities) =="
curl -s --cacert "$WORK/server.pem" https://localhost:8443/api/items; echo
echo "== GET /api/bods (stored BOD records) =="
curl -s --cacert "$WORK/server.pem" https://localhost:8443/api/bods; echo

echo "E2E OK"
