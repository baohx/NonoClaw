"""Bubble sort — a minimal, self-contained implementation.

Bubble sort repeatedly steps through the list, compares adjacent elements,
and swaps them when they are out of order. Each full pass bubbles the next
largest element to the end, so the sorted suffix grows by one each pass.

Time:  O(n^2) worst/average, O(n) best (already sorted, with early exit)
Space: O(1) extra
"""

from typing import List, TypeVar

T = TypeVar("T")


def bubble_sort(items: List[T]) -> List[T]:
    """Return a new list with `items` sorted in ascending order (stable)."""
    arr = list(items)
    n = len(arr)
    for i in range(n - 1):
        swapped = False
        # The last i elements are already in their final position.
        for j in range(n - 1 - i):
            if arr[j] > arr[j + 1]:
                arr[j], arr[j + 1] = arr[j + 1], arr[j]
                swapped = True
        if not swapped:
            break
    return arr


def bubble_sort_inplace(items: List[T]) -> None:
    """Sort `items` in place (stable)."""
    n = len(items)
    for i in range(n - 1):
        swapped = False
        for j in range(n - 1 - i):
            if items[j] > items[j + 1]:
                items[j], items[j + 1] = items[j + 1], items[j]
                swapped = True
        if not swapped:
            break


if __name__ == "__main__":
    samples = [
        [64, 34, 25, 12, 22, 11, 90],
        [5, 1, 4, 2, 8],
        [3],
        [],
        [1, 2, 3, 4, 5],
    ]
    for s in samples:
        print(f"{s} -> {bubble_sort(s)}")
