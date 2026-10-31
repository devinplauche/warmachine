// bod-sender: read an XML BOD file, convert it to the transport DTO, and
// POST it over HTTPS to the C# BodBridge API.
//
// Usage:
//   bod-sender <file.xml> --url https://host:port/api/bods
//              [--ca cert.pem] [--cert client.pem --key client-key.pem]
//              [--api-key KEY] [--print-json] [--insecure-localhost-only]
//
// Endpoint and credentials can also come from the environment:
//   BODBRIDGE_URL, BODBRIDGE_CA_CERT, BODBRIDGE_API_KEY
#include <cstdlib>
#include <iostream>
#include <memory>
#include <string>

#include "BodParser.h"
#include "HttpsClient.h"
#include "TransportMapper.h"

namespace {
std::string envOr(const char* name, const std::string& fallback = "") {
    const char* v = std::getenv(name);
    return v ? v : fallback;
}
}  // namespace

int main(int argc, char** argv) {
    if (argc < 2) {
        std::cerr << "usage: bod-sender <file.xml> --url URL [--ca cert.pem] "
                     "[--print-json] [--insecure-localhost-only]\n";
        return 2;
    }
    std::string file = argv[1];
    HttpsOptions opts;
    opts.url = envOr("BODBRIDGE_URL", "https://localhost:8443/api/bods");
    opts.caCertPath = envOr("BODBRIDGE_CA_CERT");
    opts.apiKey = envOr("BODBRIDGE_API_KEY");
    bool printJson = false;

    for (int i = 2; i < argc; ++i) {
        std::string a = argv[i];
        auto next = [&]() -> std::string { return i + 1 < argc ? argv[++i] : ""; };
        if (a == "--url") opts.url = next();
        else if (a == "--ca") opts.caCertPath = next();
        else if (a == "--cert") opts.clientCertPath = next();
        else if (a == "--key") opts.clientKeyPath = next();
        else if (a == "--api-key") opts.apiKey = next();
        else if (a == "--print-json") printJson = true;
        else if (a == "--insecure-localhost-only") opts.insecureSkipVerify = true;
        else { std::cerr << "unknown argument: " << a << "\n"; return 2; }
    }

    if (opts.insecureSkipVerify &&
        opts.url.find("localhost") == std::string::npos &&
        opts.url.find("127.0.0.1") == std::string::npos) {
        std::cerr << "refusing --insecure-localhost-only with a non-localhost URL\n";
        return 2;
    }

    try {
        // Plug points 1 and 2: swap either implementation here, nothing else changes.
        std::unique_ptr<IBodParser> parser = std::make_unique<LibXmlBodParser>();
        std::unique_ptr<ITransportMapper> mapper = std::make_unique<GenericTransportMapper>();

        auto bod = parser->parseFile(file);
        std::string json = mapper->toJson(*bod);
        if (printJson) std::cout << json << "\n";

        HttpsClient client;
        HttpsResult res = client.postJson(opts, json);
        std::cout << "HTTP " << res.status << "\n" << res.body << "\n";
        return (res.status >= 200 && res.status < 300) ? 0 : 1;
    } catch (const std::exception& e) {
        std::cerr << "error: " << e.what() << "\n";
        return 1;
    }
}
