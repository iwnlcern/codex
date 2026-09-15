#!/usr/bin/env python3

import collections
import copy
import hashlib
import pathlib
import sys
import tomllib


ALLOWED_OLD_VERSION = "0.0.0"
ALLOWED_NEW_VERSION = "0.154.0"


def fail(message: str) -> None:
    print(f"error: {message}", file=sys.stderr)
    raise SystemExit(1)


def load_document(path: str) -> tuple[bytes, dict[str, object]]:
    try:
        data = pathlib.Path(path).read_bytes()
        document = tomllib.loads(data.decode("utf-8"))
    except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
        fail(f"cannot parse {path}: {error}")

    if not isinstance(document, dict):
        fail(f"{path}: root document is malformed")
    packages = document.get("package")
    if not isinstance(packages, list) or not all(
        isinstance(package, dict) for package in packages
    ):
        fail(f"{path}: package population is missing or malformed")
    return data, document


def canonical(value: object) -> str:
    if isinstance(value, dict):
        return repr([(key, canonical(item)) for key, item in sorted(value.items())])
    if isinstance(value, list):
        return repr([canonical(item) for item in value])
    return repr(value)


def normalized_package(package: dict[str, object], *, after: bool) -> dict[str, object]:
    result = copy.deepcopy(package)
    if "source" not in result:
        version = result.get("version")
        expected = ALLOWED_NEW_VERSION if after else ALLOWED_OLD_VERSION
        if version == expected:
            result["version"] = ALLOWED_OLD_VERSION
    return result


def main() -> None:
    if len(sys.argv) != 3:
        fail(f"usage: {pathlib.Path(sys.argv[0]).name} BEFORE AFTER")

    _, before = load_document(sys.argv[1])
    after_bytes, after = load_document(sys.argv[2])
    before_non_package = copy.deepcopy(before)
    after_non_package = copy.deepcopy(after)
    before_packages = before_non_package.pop("package")
    after_packages = after_non_package.pop("package")
    assert isinstance(before_packages, list)
    assert isinstance(after_packages, list)
    before_normalized = collections.Counter(
        canonical(normalized_package(package, after=False))
        for package in before_packages
    )
    after_normalized = collections.Counter(
        canonical(normalized_package(package, after=True)) for package in after_packages
    )

    if (
        canonical(before_non_package) != canonical(after_non_package)
        or before_normalized != after_normalized
    ):
        before_by_name = collections.defaultdict(list)
        after_by_name = collections.defaultdict(list)
        for package in before_packages:
            assert isinstance(package, dict)
            normalized = normalized_package(package, after=False)
            before_by_name[package.get("name")].append(canonical(normalized))
        for package in after_packages:
            assert isinstance(package, dict)
            normalized = normalized_package(package, after=True)
            after_by_name[package.get("name")].append(canonical(normalized))
        changed_names = sorted(
            str(name)
            for name in before_by_name.keys() | after_by_name.keys()
            if collections.Counter(before_by_name[name])
            != collections.Counter(after_by_name[name])
        )
        detail = ", ".join(changed_names) if changed_names else "non-package data"
        fail(f"lockfile delta rejected; changed: {detail}")

    print(f"clean sha256={hashlib.sha256(after_bytes).hexdigest()}")


if __name__ == "__main__":
    main()
