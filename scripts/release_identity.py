"""Resolve the public Git identity shared by the release commands."""

import os
import re
import subprocess
import sys
import tomllib


class ReleaseIdentityError(ValueError):
    """The public Git identity is missing or cannot be used."""


def local_value(repo, key, env):
    if repo is None:
        return ""
    result = subprocess.run(
        ["git", "-C", repo, "config", "--local", "--null", "--get", key],
        env=env, stdout=subprocess.PIPE,
    )
    if result.returncode == 1:
        return ""
    result.check_returncode()
    return os.fsdecode(result.stdout.removesuffix(b"\0"))


def public_identity(repo, settings_file, env):
    try:
        with settings_file.open("rb") as source:
            settings = tomllib.load(source)
    except FileNotFoundError:
        settings = {}

    name, email = (
        settings[setting] if setting in settings else local_value(repo, config, env)
        for setting, config in (("release_name", "user.name"), ("release_email", "user.email"))
    )
    if not all(isinstance(value, str) and value.strip() for value in (name, email)):
        raise ReleaseIdentityError(
            "public Git identity is incomplete\n"
            "  Reason  The public Git name and email must both be nonempty strings.\n"
            "  Next    Set release_name and release_email in maintainer settings or "
            "configure user.name and user.email in the public clone with git config --local."
        )
    return name, email


def is_noreply_email(email):
    return re.fullmatch(r"[^@\s<>]+@users\.noreply\.github\.com", email, re.IGNORECASE | re.ASCII) is not None


def release_identity(repo, settings_file, env, allow_email):
    name, email = public_identity(repo, settings_file, env)
    if not is_noreply_email(email):
        if not allow_email:
            raise ReleaseIdentityError(
                "refusing to use the public Git email\n"
                "  Reason  The configured email is outside the GitHub no-reply domain.\n"
                "  Next    Set release_email or the public clone's local user.email to your "
                "GitHub no-reply address, or pass --allow-email to use the configured address."
            )
        print(f"warning: --allow-email permits {email!r} as the public Git email", file=sys.stderr)
    return name, email


def identity_environment(env, identity):
    name, email = identity
    return {
        **env,
        "GIT_AUTHOR_NAME": name,
        "GIT_AUTHOR_EMAIL": email,
        "GIT_COMMITTER_NAME": name,
        "GIT_COMMITTER_EMAIL": email,
    }
