#!/bin/bash

find crates -maxdepth 1 -type d | grep '/' | while read dir; do
    pushd "${dir}"
    cargo publish --dry-run
    popd
done