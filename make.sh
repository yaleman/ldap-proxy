cargo vendor 1> ./cargo_config
docker buildx build --pull --push --platform "linux/amd64" -f ./Dockerfile -t firstyear/ldap_proxy:latest .