cmake_minimum_required(VERSION 3.16)
project(bod_sender CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

find_package(CURL REQUIRED)
find_package(LibXml2 REQUIRED)

add_executable(bod-sender
    src/main.cpp
    src/BodParser.cpp
    src/TransportMapper.cpp
    src/HttpsClient.cpp)

target_link_libraries(bod-sender PRIVATE CURL::libcurl LibXml2::LibXml2)
target_compile_options(bod-sender PRIVATE -Wall -Wextra)
