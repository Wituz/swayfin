#!/bin/sh
# Run by xdg-desktop-portal-termfilechooser (see its man page, xdg-desktop-portal-termfilechooser(5)):
# $1 multiple, $2 directory, $3 save, $4 suggested path, $5 output file.
if [ "$3" = 1 ]; then mode=save
elif [ "$2" = 1 ]; then mode=directory
elif [ "$1" = 1 ]; then mode=multiple
else mode=open
fi
exec "$(dirname "$(realpath "$0")")/../target/release/swayfin" --choose "$mode" "$4" "$5"
