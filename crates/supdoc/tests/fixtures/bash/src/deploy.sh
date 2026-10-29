#!/bin/bash

MAX_RETRIES=3
DEPLOY_DIR="/opt/app"

setup_env() {
    export PATH="$DEPLOY_DIR/bin:$PATH"
    mkdir -p "$DEPLOY_DIR"
}

function deploy_app {
    local version="$1"
    cp -r build/* "$DEPLOY_DIR/"
    echo "Deployed version $version"
}

run_tests() {
    cd tests && ./run.sh
}
