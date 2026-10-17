set -e
cc -o "$ARTIFACT.user" user.c $RUSTD_OBJECTS "$ARTIFACT" -Wl,--allow-multiple-definition -lpthread -ldl -lm
LD_LIBRARY_PATH="$(dirname "$ARTIFACT"):$LD_LIBRARY_PATH" "$ARTIFACT.user"
