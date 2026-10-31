#pragma once
#include <map>
#include <memory>
#include <string>
#include <vector>

// Generic XML element tree. Deliberately schema-free: ANY XML BOD can be
// loaded into this shape, so no proprietary noun/schema is baked into the
// sender. Typed interpretation happens in TransportMapper (C++ plug point)
// and in the C# IBodMapper (server plug point).
struct XmlNode {
    std::string name;                                   // local name, namespace prefix stripped
    std::string text;                                   // trimmed character content
    std::map<std::string, std::string> attributes;
    std::vector<std::shared_ptr<XmlNode>> children;

    const XmlNode* find(const std::string& childName) const;
    std::string childText(const std::string& childName, const std::string& fallback = "") const;
};

// ============================================================================
// PLUG POINT 1 (XML format): IBodParser
// Swap this implementation when your BOD dialect needs something libxml2's
// generic DOM walk does not give you (e.g. a generated parser from an XSD,
// streaming SAX for very large BODs, signature verification, ...).
// Everything downstream consumes XmlNode and does not care.
// ============================================================================
class IBodParser {
public:
    virtual ~IBodParser() = default;
    virtual std::shared_ptr<XmlNode> parseFile(const std::string& path) = 0;
};

class LibXmlBodParser : public IBodParser {
public:
    std::shared_ptr<XmlNode> parseFile(const std::string& path) override;
};
