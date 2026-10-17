set -e
(cd upstream && git init -q && git add -A && git -c user.name=t -c user.email=t@t commit -qm first)
sed "s|@HERE@|$PWD|" app/Cargo.toml.in > app/Cargo.toml
sed "s|@HERE@|$PWD|" Cargo.toml.in > Cargo.toml
