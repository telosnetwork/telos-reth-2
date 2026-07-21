#!/usr/bin/env python3
"""Exhaustive crash-boundary model for the five-object restore journal."""

from __future__ import annotations

from dataclasses import dataclass, replace
from itertools import product


@dataclass(frozen=True)
class ObjectState:
    target: str | None
    staged: str | None
    old: str | None = None
    failed: str | None = None


@dataclass(frozen=True)
class Transaction:
    status: str
    objects: tuple[ObjectState, ...]


def initial(had_original: tuple[bool, ...]) -> Transaction:
    return Transaction(
        "none",
        tuple(ObjectState("old" if had else None, "new") for had in had_original),
    )


def publish_states(had_original: tuple[bool, ...]) -> list[Transaction]:
    transaction = replace(initial(had_original), status="pending")
    states = [transaction]
    objects = list(transaction.objects)
    for index, had in enumerate(had_original):
        current = objects[index]
        if had:
            assert current.target == "old" and current.old is None
            objects[index] = replace(current, target=None, old="old")
            states.append(Transaction("pending", tuple(objects)))
            current = objects[index]
        assert current.target is None and current.staged == "new"
        objects[index] = replace(current, target="new", staged=None)
        states.append(Transaction("pending", tuple(objects)))
    return states


def recovery_attempt(
    transaction: Transaction, had_original: tuple[bool, ...]
) -> list[Transaction]:
    """Return every durable state after one recovery action, including terminal state."""
    assert transaction.status == "pending"
    objects = list(transaction.objects)
    states: list[Transaction] = []
    for index in reversed(range(len(objects))):
        current = objects[index]
        if had_original[index]:
            if current.old is not None:
                if current.target is not None:
                    assert current.failed is None
                    objects[index] = replace(current, target=None, failed=current.target)
                    states.append(Transaction("pending", tuple(objects)))
                    current = objects[index]
                assert current.target is None and current.old == "old"
                objects[index] = replace(current, target="old", old=None)
                states.append(Transaction("pending", tuple(objects)))
            else:
                assert current.target == "old"
        else:
            assert current.old is None
            if current.target is not None:
                assert current.failed is None
                assert current.staged is None or current.target == "systemd-empty"
                objects[index] = replace(current, target=None, failed=current.target)
                states.append(Transaction("pending", tuple(objects)))

    for index, had in enumerate(had_original):
        assert objects[index].target == ("old" if had else None)
        assert objects[index].old is None
    states.append(Transaction("rolled_back", tuple(objects)))
    return states


def assert_recovery_survives_every_crash(
    start: Transaction, had_original: tuple[bool, ...]
) -> int:
    pending = [start]
    seen: set[Transaction] = set()
    checked = 0
    while pending:
        transaction = pending.pop()
        if transaction in seen:
            continue
        seen.add(transaction)
        checked += 1
        outcomes = recovery_attempt(transaction, had_original)
        terminal = outcomes[-1]
        assert terminal.status == "rolled_back"
        for index, had in enumerate(had_original):
            assert terminal.objects[index].target == ("old" if had else None)
        pending.extend(outcome for outcome in outcomes[:-1] if outcome.status == "pending")
    return checked


def main() -> None:
    checked = 0
    # No object rename is permitted until the pending journal is durable.
    for bitmap in product((False, True), repeat=5):
        unfenced = initial(bitmap)
        assert unfenced.status == "none"
        assert all(obj.old is None and obj.failed is None for obj in unfenced.objects)

        publication = publish_states(bitmap)
        for crash_state in publication:
            checked += assert_recovery_survives_every_crash(crash_state, bitmap)
            # A rejected boot-time start can still let systemd create an originally absent
            # StateDirectory before ExecStartPre observes the journal.
            for index in (0, 1):
                current = crash_state.objects[index]
                if not bitmap[index] and current.target is None and current.staged == "new":
                    objects = list(crash_state.objects)
                    objects[index] = replace(current, target="systemd-empty")
                    checked += assert_recovery_survives_every_crash(
                        replace(crash_state, objects=tuple(objects)), bitmap
                    )

        fully_published = publication[-1]
        assert all(obj.target == "new" and obj.staged is None for obj in fully_published.objects)
        committed = replace(fully_published, status="committed")
        assert committed.status == "committed"

    print(f"restore journal crash model: {checked} pending states verified")


if __name__ == "__main__":
    main()
