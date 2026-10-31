#pragma once
#include <string>

#include "BodParser.h"

// Converts a parsed BOD (XmlNode tree) into the JSON transport DTO defined
// in contracts/bod-transport.schema.json. The DTO -- not an EF Core object --
// is what crosses HTTPS, because EF Core entities cannot leave the C#
// process. The C# side maps this DTO onto its EF Core entities.
//
// ============================================================================
// PLUG POINT 2 (noun mapping, C++ side): ITransportMapper
// This default implementation is fully generic: header fields are lifted
// from ApplicationArea, and the noun under DataArea is converted
// element-for-element (repeated elements become JSON arrays, attributes
// become "@name"). Replace it if your wire format should be a hand-shaped,
// typed DTO instead of the generic noun mirror.
// ============================================================================
class ITransportMapper {
public:
    virtual ~ITransportMapper() = default;
    virtual std::string toJson(const XmlNode& bodRoot) = 0;
};

class GenericTransportMapper : public ITransportMapper {
public:
    std::string toJson(const XmlNode& bodRoot) override;
};

// Generic XML -> JSON conversion of one element (used for the noun payload).
std::string elementToJson(const XmlNode& node);
