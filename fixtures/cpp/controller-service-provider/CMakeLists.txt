cmake_minimum_required(VERSION 3.20)
project(chakra_cpp_fixture LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 20)
add_library(payments
    src/payment_provider.cpp
    src/payment_service.cpp
    src/payment_controller.cpp
)
target_include_directories(payments PUBLIC include)
