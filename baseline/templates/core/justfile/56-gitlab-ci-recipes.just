
# Lint .gitlab-ci.yml on the server. Needs glab, a login, and a remote, so it
# is a hook-only check and not part of `lint`.
lint-gitlab-ci:
    glab ci lint .gitlab-ci.yml
