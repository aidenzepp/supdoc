# Deployment Guide

The max retry count is [`MAX_RETRIES`](../src/deploy.sh#MAX_RETRIES).

The deploy directory is [`DEPLOY_DIR`](../src/deploy.sh#DEPLOY_DIR).

The [`setup_env`](../src/deploy.sh#setup_env) function prepares the environment.

The [`deploy_app`](../src/deploy.sh#deploy_app) function copies build artifacts.

The [`run_tests`](../src/deploy.sh#run_tests) function executes the test suite.
