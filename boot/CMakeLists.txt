# Builds only the ESP-IDF 2nd stage bootloader and the partition table for
# tab5-os (`idf.py bootloader partition-table`). The app part is a stub; the
# real "app" is the Rust kernel. See doc/07 §2 and doc/05.
cmake_minimum_required(VERSION 3.22)
include($ENV{IDF_PATH}/tools/cmake/project.cmake)
idf_build_set_property(MINIMAL_BUILD ON)
project(tab5_os_boot)
