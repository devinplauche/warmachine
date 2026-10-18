# vhms-maintenance-ui - OpenGL SC 2.0 vehicle maintenance dashboard
cmake_minimum_required(VERSION 3.16)
project(vhms_maintenance_ui C)

option(VHMS_UI_DEV_BUILD
    "Build against desktop GLES2/EGL for development (ON) or the OpenGL SC 2.0 target toolchain (OFF)"
    ON)

set(CMAKE_C_STANDARD 99)
set(CMAKE_C_STANDARD_REQUIRED ON)
set(CMAKE_C_EXTENSIONS OFF)

add_executable(vhms_maintenance_ui
    src/main.c
    src/shaders.c
    src/ui_core.c
    src/data_model.c
    src/screens.c
    src/platform_egl.c
)

# Safety-critical coding baseline: warnings are errors in every build.
target_compile_options(vhms_maintenance_ui PRIVATE -Wall -Wextra -Werror)

if(VHMS_UI_DEV_BUILD)
    target_compile_definitions(vhms_maintenance_ui PRIVATE VHMS_UI_DEV_BUILD)
    find_path(GLES2_INCLUDE_DIR GLES2/gl2.h REQUIRED)
    find_library(GLES2_LIBRARY GLESv2 REQUIRED)
    find_library(EGL_LIBRARY EGL REQUIRED)
    target_include_directories(vhms_maintenance_ui PRIVATE ${GLES2_INCLUDE_DIR})
    target_link_libraries(vhms_maintenance_ui PRIVATE ${GLES2_LIBRARY} ${EGL_LIBRARY})
else()
    # ---- OpenGL SC 2.0 target build ----
    # The target toolchain must provide:
    #   VHMS_UI_SC_INCLUDE_DIR : directory containing the OpenGL SC 2.0
    #                            header (GL/glsc2.h as shipped by the SC
    #                            implementation)
    #   VHMS_UI_SC_LIBRARY     : the OpenGL SC 2.0 implementation library
    # Example:
    #   cmake -DVHMS_UI_DEV_BUILD=OFF \
    #         -DVHMS_UI_SC_INCLUDE_DIR=/opt/sc-sdk/include \
    #         -DVHMS_UI_SC_LIBRARY=/opt/sc-sdk/lib/libGLSC2.a ..
    # NOTE: platform_egl.c is the dev/EGL implementation and intentionally
    # fails the SC build with #error - replace it with the platform display
    # driver (see src/platform_egl.h for the contract).
    set(VHMS_UI_SC_INCLUDE_DIR "" CACHE PATH
        "Include dir for the OpenGL SC 2.0 header (GL/glsc2.h)")
    set(VHMS_UI_SC_LIBRARY "" CACHE FILEPATH
        "OpenGL SC 2.0 implementation library")
    if(NOT VHMS_UI_SC_INCLUDE_DIR OR NOT VHMS_UI_SC_LIBRARY)
        message(FATAL_ERROR
            "VHMS_UI_DEV_BUILD=OFF requires VHMS_UI_SC_INCLUDE_DIR and "
            "VHMS_UI_SC_LIBRARY (see comments above).")
    endif()
    target_include_directories(vhms_maintenance_ui PRIVATE ${VHMS_UI_SC_INCLUDE_DIR})
    target_link_libraries(vhms_maintenance_ui PRIVATE ${VHMS_UI_SC_LIBRARY})
endif()
