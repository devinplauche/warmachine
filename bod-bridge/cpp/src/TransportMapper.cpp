#include "TransportMapper.h"

#include <map>
#include <sstream>

namespace {

std::string jsonEscape(const std::string& s) {
    std::string out;
    for (char c : s) {
        switch (c) {
            case '"': out += "\\\""; break;
            case '\\': out += "\\\\"; break;
            case '\n': out += "\\n"; break;
            case '\r': out += "\\r"; break;
            case '\t': out += "\\t"; break;
            default:
                if (static_cast<unsigned char>(c) < 0x20) {
                    char buf[8];
                    std::snprintf(buf, sizeof(buf), "\\u%04x", c);
                    out += buf;
                } else {
                    out += c;
                }
        }
    }
    return out;
}

std::string quoted(const std::string& s) { return "\"" + jsonEscape(s) + "\""; }

}  // namespace

std::string elementToJson(const XmlNode& node) {
    if (node.children.empty() && node.attributes.empty()) return quoted(node.text);

    // Group children by name so repeated elements become arrays.
    std::map<std::string, std::vector<const XmlNode*>> byName;
    std::vector<std::string> order;
    for (const auto& c : node.children) {
        if (byName.count(c->name) == 0) order.push_back(c->name);
        byName[c->name].push_back(c.get());
    }

    std::ostringstream o;
    o << "{";
    bool first = true;
    auto sep = [&]() {
        if (!first) o << ",";
        first = false;
    };
    for (const auto& [k, v] : node.attributes) {
        sep();
        o << quoted("@" + k) << ":" << quoted(v);
    }
    if (!node.text.empty()) {
        sep();
        o << quoted("#text") << ":" << quoted(node.text);
    }
    for (const auto& name : order) {
        sep();
        o << quoted(name) << ":";
        const auto& group = byName[name];
        if (group.size() == 1) {
            o << elementToJson(*group[0]);
        } else {
            o << "[";
            for (size_t i = 0; i < group.size(); ++i) {
                if (i) o << ",";
                o << elementToJson(*group[i]);
            }
            o << "]";
        }
    }
    o << "}";
    return o.str();
}

std::string GenericTransportMapper::toJson(const XmlNode& bodRoot) {
    const XmlNode* app = bodRoot.find("ApplicationArea");
    const XmlNode* data = bodRoot.find("DataArea");

    std::string bodId = app ? app->childText("BODID") : "";
    if (bodId.empty()) bodId = "cpp-" + std::to_string(std::hash<std::string>{}(bodRoot.name));
    std::string created = app ? app->childText("CreationDateTime") : "";
    std::string sender;
    if (app) {
        if (const XmlNode* s = app->find("Sender")) sender = s->childText("LogicalID");
    }

    // DataArea's first child is the verb, its second child is the noun.
    std::string verb = "Unknown";
    const XmlNode* noun = nullptr;
    if (data && !data->children.empty()) {
        verb = data->children[0]->name;
        if (data->children.size() > 1) noun = data->children[1].get();
    }
    if (!noun) throw std::runtime_error("BOD has no noun element under DataArea");

    std::ostringstream o;
    o << "{";
    o << "\"bodId\":" << quoted(bodId) << ",";
    o << "\"verb\":" << quoted(verb) << ",";
    o << "\"nounType\":" << quoted(noun->name) << ",";
    o << "\"senderId\":" << (sender.empty() ? std::string("null") : quoted(sender)) << ",";
    o << "\"creationDateTime\":" << (created.empty() ? std::string("null") : quoted(created)) << ",";
    o << "\"payload\":" << elementToJson(*noun);
    o << "}";
    return o.str();
}
