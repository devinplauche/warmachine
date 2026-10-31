#pragma once
#include <string>

struct HttpsOptions {
    std::string url;             // e.g. https://localhost:8443/api/bods
    std::string caCertPath;      // CA bundle / self-signed server cert to trust ("" = system store)
    std::string clientCertPath;  // optional mTLS client certificate (PEM)
    std::string clientKeyPath;   // optional mTLS client key (PEM)
    std::string apiKey;          // optional, sent as X-Api-Key (or swap for your auth scheme)
    bool insecureSkipVerify = false;  // LOCAL DEV ONLY. Never ship production with this on.
};

struct HttpsResult {
    long status = 0;
    std::string body;
};

// Thin libcurl wrapper. TLS verification is ON by default; the only way to
// turn it off is the explicit flag above, which main() refuses to combine
// with a non-localhost URL.
class HttpsClient {
public:
    HttpsResult postJson(const HttpsOptions& opts, const std::string& json);
};
