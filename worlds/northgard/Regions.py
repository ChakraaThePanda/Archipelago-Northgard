# The fixed Conquest-mode branching tree: position in the tree, independent of which
# clans actually occupy each node in a given real playthrough.
CHAPTER_CONNECTIONS: dict[str, list[str]] = {
    "Chapter 01": ["Chapter 02 - Top", "Chapter 02 - Bottom"],
    "Chapter 02 - Top": ["Chapter 03 - Top", "Chapter 03 - Bottom"],
    "Chapter 02 - Bottom": ["Chapter 03 - Top", "Chapter 03 - Bottom"],
    "Chapter 03 - Top": ["Chapter 04"],
    "Chapter 03 - Bottom": ["Chapter 04"],
    "Chapter 04": ["Chapter 05 - Top", "Chapter 05 - Bottom"],
    "Chapter 05 - Top": ["Chapter 06 - Top", "Chapter 06 - Bottom"],
    "Chapter 05 - Bottom": ["Chapter 06 - Top", "Chapter 06 - Bottom"],
    "Chapter 06 - Top": ["Chapter 07"],
    "Chapter 06 - Bottom": ["Chapter 07"],
    "Chapter 07": [],
}

CHAPTERS: list[str] = list(CHAPTER_CONNECTIONS.keys())

STARTING_CHAPTER = "Chapter 01"
FINAL_CHAPTER = "Chapter 07"
