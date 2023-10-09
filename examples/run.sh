#! /usr/bin/env bash

rm -f {squares,landscape,portrait,mixed,steps}/*

find source -type f | ../target/release/snapcrop squares --res 256
find source -type f | ../target/release/snapcrop landscape --res 256x128
find source -type f | ../target/release/snapcrop portrait --res 128x256
find source -type f | ../target/release/snapcrop mixed --res "256,[128x256]"
find source -type f | ../target/release/snapcrop steps --res "[128:256:8x128:256:8]"