cmake_minimum_required(VERSION 3.20)
project(swayfin-thumbd LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

find_package(Qt6 REQUIRED COMPONENTS Gui)
find_package(KF6KIO REQUIRED)

add_executable(swayfin-thumbd main.cpp)
target_link_libraries(swayfin-thumbd PRIVATE Qt6::Gui KF6::KIOGui KF6::KIOCore)
