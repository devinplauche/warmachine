#include "HttpsClient.h"

#include <curl/curl.h>

#include <stdexcept>

namespace {
size_t writeCb(char* ptr, size_t size, size_t nmemb, void* userdata) {
    auto* out = static_cast<std::string*>(userdata);
    out->append(ptr, size * nmemb);
    return size * nmemb;
}
}  // namespace

HttpsResult HttpsClient::postJson(const HttpsOptions& opts, const std::string& json) {
    CURL* curl = curl_easy_init();
    if (!curl) throw std::runtime_error("curl_easy_init failed");
    struct Guard {
        CURL* c;
        curl_slist* h = nullptr;
        ~Guard() {
            if (h) curl_slist_free_all(h);
            curl_easy_cleanup(c);
        }
    } guard{curl};

    HttpsResult result;
    curl_easy_setopt(curl, CURLOPT_URL, opts.url.c_str());
    curl_easy_setopt(curl, CURLOPT_POST, 1L);
    curl_easy_setopt(curl, CURLOPT_POSTFIELDS, json.c_str());
    curl_easy_setopt(curl, CURLOPT_POSTFIELDSIZE, static_cast<long>(json.size()));
    curl_easy_setopt(curl, CURLOPT_WRITEFUNCTION, writeCb);
    curl_easy_setopt(curl, CURLOPT_WRITEDATA, &result.body);
    curl_easy_setopt(curl, CURLOPT_TIMEOUT, 30L);

    if (opts.insecureSkipVerify) {
        curl_easy_setopt(curl, CURLOPT_SSL_VERIFYPEER, 0L);
        curl_easy_setopt(curl, CURLOPT_SSL_VERIFYHOST, 0L);
    }
    if (!opts.caCertPath.empty()) curl_easy_setopt(curl, CURLOPT_CAINFO, opts.caCertPath.c_str());
    if (!opts.clientCertPath.empty()) curl_easy_setopt(curl, CURLOPT_SSLCERT, opts.clientCertPath.c_str());
    if (!opts.clientKeyPath.empty()) curl_easy_setopt(curl, CURLOPT_SSLKEY, opts.clientKeyPath.c_str());

    guard.h = curl_slist_append(guard.h, "Content-Type: application/json");
    if (!opts.apiKey.empty())
        guard.h = curl_slist_append(guard.h, ("X-Api-Key: " + opts.apiKey).c_str());
    curl_easy_setopt(curl, CURLOPT_HTTPHEADER, guard.h);

    CURLcode rc = curl_easy_perform(curl);
    if (rc != CURLE_OK)
        throw std::runtime_error(std::string("HTTPS POST failed: ") + curl_easy_strerror(rc));
    curl_easy_getinfo(curl, CURLINFO_RESPONSE_CODE, &result.status);
    return result;
}
