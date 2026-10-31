#include "BodParser.h"

#include <libxml/parser.h>
#include <libxml/tree.h>

#include <stdexcept>

const XmlNode* XmlNode::find(const std::string& childName) const {
    for (const auto& c : children)
        if (c->name == childName) return c.get();
    return nullptr;
}

std::string XmlNode::childText(const std::string& childName, const std::string& fallback) const {
    const XmlNode* c = find(childName);
    return c ? c->text : fallback;
}

namespace {

std::string localName(const xmlChar* name) {
    std::string n = reinterpret_cast<const char*>(name);
    auto colon = n.find(':');
    return colon == std::string::npos ? n : n.substr(colon + 1);
}

std::string trim(const std::string& s) {
    auto b = s.find_first_not_of(" \t\r\n");
    if (b == std::string::npos) return "";
    auto e = s.find_last_not_of(" \t\r\n");
    return s.substr(b, e - b + 1);
}

std::shared_ptr<XmlNode> convert(xmlNode* x) {
    auto node = std::make_shared<XmlNode>();
    node->name = localName(x->name);
    for (xmlAttr* a = x->properties; a; a = a->next) {
        xmlChar* v = xmlNodeListGetString(x->doc, a->children, 1);
        node->attributes[localName(a->name)] = v ? reinterpret_cast<const char*>(v) : "";
        if (v) xmlFree(v);
    }
    std::string text;
    for (xmlNode* c = x->children; c; c = c->next) {
        if (c->type == XML_ELEMENT_NODE) {
            node->children.push_back(convert(c));
        } else if (c->type == XML_TEXT_NODE && c->content) {
            text += reinterpret_cast<const char*>(c->content);
        }
    }
    node->text = trim(text);
    return node;
}

}  // namespace

std::shared_ptr<XmlNode> LibXmlBodParser::parseFile(const std::string& path) {
    xmlDoc* doc = xmlReadFile(path.c_str(), nullptr, 0);
    if (!doc) throw std::runtime_error("failed to parse XML file: " + path);
    xmlNode* root = xmlDocGetRootElement(doc);
    auto tree = convert(root);
    xmlFreeDoc(doc);
    return tree;
}
