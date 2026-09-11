#!/usr/bin/env python3
"""시나리오(.uit) 파일을 셸이 읽기 쉬운 토큰 스트림으로 편다.

한 줄에 한 명령이고, 출력은 명령마다 한 줄이며 토큰은 US(\\x1f)로 구분한다.
US 는 셸 인용 규칙과 겹치지 않아 공백이 든 인자를 그대로 넘길 수 있다.

문법
  # 주석            줄 전체 주석 (줄 맨 앞, 앞쪽 공백 허용)
  set 이름 값…      변수 정의(기본값). 명령줄에서 `이름=값` 으로 준 것이 있으면 그쪽이 이긴다.
  include 다른.uit  다른 시나리오를 이 자리에 펼친다 (포함하는 파일 기준 상대 경로).
  $이름             토큰 안 어디서나 치환. ${이름} 도 된다. 없는 변수는 오류.
  "따옴표"          shlex 규칙 — 공백이 든 인자는 따옴표로 감싼다.

표준 출력에 토큰 스트림, 오류는 표준 오류에 쓰고 종료 코드 1.
"""

import os
import re
import shlex
import sys

US = "\x1f"
MAX_DEPTH = 8
VAR = re.compile(r"\$(\{)?([A-Za-z_][A-Za-z0-9_]*)(?(1)\})")


class ScenarioError(Exception):
    pass


def substitute(token: str, variables: dict, where: str) -> str:
    """토큰 안의 $이름 / ${이름} 을 한 번만 훑어 치환한다."""

    def repl(m: re.Match) -> str:
        name = m.group(2)
        if name not in variables:
            raise ScenarioError(f"{where}: 정의되지 않은 변수 ${name}")
        return variables[name]

    return VAR.sub(repl, token)


def expand(path: str, variables: dict, locked: set, depth: int, seen: list) -> list:
    if depth > MAX_DEPTH:
        raise ScenarioError(f"include 가 너무 깊습니다 ({MAX_DEPTH} 단계): {path}")
    real = os.path.realpath(path)
    if real in seen:
        chain = " → ".join(os.path.basename(p) for p in seen + [real])
        raise ScenarioError(f"include 가 순환합니다: {chain}")

    try:
        with open(path, encoding="utf-8") as f:
            raw = f.read().splitlines()
    except OSError as e:
        raise ScenarioError(f"시나리오를 읽지 못했습니다 ({path}): {e}") from e

    out = []
    for lineno, line in enumerate(raw, 1):
        where = f"{os.path.basename(path)}:{lineno}"
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        try:
            tokens = shlex.split(line, comments=False)
        except ValueError as e:
            raise ScenarioError(f"{where}: 따옴표가 맞지 않습니다 ({e})")
        if not tokens:
            continue

        tokens = [substitute(t, variables, where) for t in tokens]
        head = tokens[0]

        if head == "set":
            if len(tokens) < 2:
                raise ScenarioError(f"{where}: set 뒤에 변수 이름이 없습니다")
            name = tokens[1]
            if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name):
                raise ScenarioError(f"{where}: 변수 이름이 잘못됐습니다: {name}")
            if name not in locked:
                variables[name] = " ".join(tokens[2:])
            continue

        if head == "include":
            if len(tokens) != 2:
                raise ScenarioError(f"{where}: include 는 파일 하나만 받습니다")
            child = tokens[1]
            if not os.path.isabs(child):
                child = os.path.join(os.path.dirname(os.path.abspath(path)), child)
            out.extend(expand(child, variables, locked, depth + 1, seen + [real]))
            continue

        # 첫 토큰(명령)과 원래 위치를 함께 흘려보내 실패 보고에 쓴다.
        out.append([where] + tokens)
    return out


def main(argv: list) -> int:
    if len(argv) < 2:
        print("사용법: parse_uit.py <시나리오.uit> [이름=값 …]", file=sys.stderr)
        return 2
    variables = {}
    # 명령줄로 준 변수는 잠근다 — 파일의 `set` 은 기본값 노릇만 한다.
    locked = set()
    for extra in argv[2:]:
        if "=" not in extra:
            print(f"이름=값 꼴이어야 합니다: {extra}", file=sys.stderr)
            return 2
        name, value = extra.split("=", 1)
        variables[name] = value
        locked.add(name)
    try:
        steps = expand(argv[1], variables, locked, 0, [])
    except ScenarioError as e:
        print(e, file=sys.stderr)
        return 1
    for step in steps:
        sys.stdout.write(US.join(step) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
