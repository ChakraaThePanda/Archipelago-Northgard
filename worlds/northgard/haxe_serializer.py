"""
Minimal codec for Haxe's haxe.Serializer / haxe.Unserializer text format, which is what
Northgard uses for its .sav files.

Decoded representation (deliberately plain Python types so callers can just treat this
like parsed JSON):
    null            -> None
    true/false      -> bool
    int             -> int
    float           -> float (including nan/inf)
    string          -> str
    array ("a")     -> list
    anon object("o")-> dict[str, Any]
    StringMap ("b") -> HaxeStringMap (dict subclass, so it still behaves like a dict)
    IntMap ("q")    -> HaxeIntMap (dict[int, Any] subclass)
    class instance  -> HaxeClassInstance(name, fields: dict)
    enum value      -> HaxeEnum(name, constructor, params: list)
    List ("l")      -> HaxeList (list subclass)

Object/array reference sharing (lowercase "r") is supported on decode for correctness,
but the encoder never emits it -- real-world Haxe output only relies on it when a type
opts into Serializer's `useCache`, which is off by default, and nothing observed in
Northgard's saves needed it. String reference sharing (uppercase "R") is emitted
normally, since Haxe always does this and it's a large fraction of file size on repetitive
key names.

This is a from-scratch reimplementation based on the documented wire format, not a port
of Haxe's own source -- treat the write path (`encode`) as the least-tested part of this
whole project. Always diff/round-trip-test before trusting it against a real save.
"""
from __future__ import annotations

import math
from dataclasses import dataclass, field
from typing import Any
from urllib.parse import quote, unquote


class HaxeStringMap(dict):
    pass


class HaxeIntMap(dict):
    pass


class HaxeList(list):
    pass


@dataclass
class HaxeClassInstance:
    name: str
    fields: dict[str, Any] = field(default_factory=dict)


@dataclass
class HaxeEnum:
    name: str
    constructor: str
    index: int
    params: list[Any] = field(default_factory=list)


class HaxeDecodeError(ValueError):
    pass


def _percent_encode(s: str) -> str:
    # Deliberately conservative (over-escape rather than under-escape) -- any %XX
    # sequence decodes unambiguously regardless of exactly which characters a real
    # Haxe encoder would have chosen to leave bare.
    return quote(s, safe="")


def _percent_decode(s: str) -> str:
    return unquote(s)


class _Decoder:
    def __init__(self, text: str):
        self.text = text
        self.pos = 0
        self.str_cache: list[str] = []
        self.obj_cache: list[Any] = []

    def _peek(self) -> str:
        if self.pos >= len(self.text):
            raise HaxeDecodeError(f"Unexpected end of input at {self.pos}")
        return self.text[self.pos]

    def _read_digits(self) -> int:
        start = self.pos
        if self._peek() == "-":
            self.pos += 1
        while self.pos < len(self.text) and self.text[self.pos].isdigit():
            self.pos += 1
        chunk = self.text[start:self.pos]
        if chunk in ("", "-"):
            raise HaxeDecodeError(f"Expected digits at {start}")
        return int(chunk)

    def _read_float_digits(self) -> float:
        start = self.pos
        while self.pos < len(self.text) and (self.text[self.pos].isdigit() or self.text[self.pos] in "+-.eE"):
            self.pos += 1
        return float(self.text[start:self.pos])

    def decode_value(self) -> Any:
        tag = self._peek()
        self.pos += 1

        if tag == "n":
            return None
        if tag == "t":
            return True
        if tag == "f":
            return False
        if tag == "z":
            return 0
        if tag == "k":
            return math.nan
        if tag == "m":
            return -math.inf
        if tag == "p":
            return math.inf
        if tag == "i":
            return self._read_digits()
        if tag == "d":
            return self._read_float_digits()
        if tag == "y":
            return self._decode_string()
        if tag == "R":
            idx = self._read_digits()
            if idx < 0 or idx >= len(self.str_cache):
                raise HaxeDecodeError(f"String cache reference out of range: R{idx}")
            return self.str_cache[idx]
        if tag == "r":
            idx = self._read_digits()
            if idx < 0 or idx >= len(self.obj_cache):
                raise HaxeDecodeError(f"Object cache reference out of range: r{idx}")
            return self.obj_cache[idx]
        if tag == "a":
            return self._decode_array()
        if tag == "o":
            return self._decode_anon_object()
        if tag == "b":
            return self._decode_map(HaxeStringMap(), key_is_string=True)
        if tag == "q":
            return self._decode_map(HaxeIntMap(), key_is_string=False)
        if tag == "l":
            return self._decode_list()
        if tag == "c":
            return self._decode_class_instance()
        if tag == "w":
            return self._decode_enum_by_name()
        if tag == "j":
            return self._decode_enum_by_index()
        if tag == "x":
            # Serialized exception: wraps a single value.
            return self.decode_value()

        raise HaxeDecodeError(f"Unhandled Haxe serializer tag {tag!r} at position {self.pos - 1}")

    def _decode_string(self) -> str:
        length = self._read_digits()
        if self._peek() != ":":
            raise HaxeDecodeError(f"Expected ':' after string length at {self.pos}")
        self.pos += 1
        raw = self.text[self.pos:self.pos + length]
        if len(raw) != length:
            raise HaxeDecodeError("Truncated string in input")
        self.pos += length
        value = _percent_decode(raw)
        self.str_cache.append(value)
        return value

    def _decode_array(self) -> list:
        result: list = []
        self.obj_cache.append(result)
        while True:
            c = self._peek()
            if c == "h":
                self.pos += 1
                break
            if c == "u":
                self.pos += 1
                n = self._read_digits()
                result.extend([None] * n)
                continue
            result.append(self.decode_value())
        return result

    def _decode_anon_object(self) -> dict:
        result: dict[str, Any] = {}
        self.obj_cache.append(result)
        while True:
            if self._peek() == "g":
                self.pos += 1
                break
            key = self.decode_value()
            if not isinstance(key, str):
                raise HaxeDecodeError(f"Anonymous object key was not a string: {key!r}")
            result[key] = self.decode_value()
        return result

    def _decode_map(self, result, key_is_string: bool):
        self.obj_cache.append(result)
        while True:
            if self._peek() == "h":
                self.pos += 1
                break
            key = self.decode_value()
            value = self.decode_value()
            result[key] = value
        return result

    def _decode_list(self) -> HaxeList:
        result = HaxeList()
        self.obj_cache.append(result)
        while True:
            if self._peek() == "h":
                self.pos += 1
                break
            result.append(self.decode_value())
        return result

    def _decode_class_instance(self) -> HaxeClassInstance:
        name = self.decode_value()
        if not isinstance(name, str):
            raise HaxeDecodeError("Class instance name was not a string")
        inst = HaxeClassInstance(name=name)
        self.obj_cache.append(inst)
        while True:
            if self._peek() == "g":
                self.pos += 1
                break
            key = self.decode_value()
            if not isinstance(key, str):
                raise HaxeDecodeError(f"Class instance field key was not a string: {key!r}")
            inst.fields[key] = self.decode_value()
        return inst

    def _decode_enum_by_name(self) -> HaxeEnum:
        # Wire format: "w" name constructor ":" nargs (values...) -- no index field.
        name = self.decode_value()
        constructor = self.decode_value()
        if not isinstance(name, str) or not isinstance(constructor, str):
            raise HaxeDecodeError("Enum name/constructor was not a string")
        if self._peek() != ":":
            raise HaxeDecodeError("Expected ':' before enum params count")
        self.pos += 1
        count = self._read_digits()
        result = HaxeEnum(name=name, constructor=constructor, index=-1)
        self.obj_cache.append(result)
        for _ in range(count):
            result.params.append(self.decode_value())
        return result

    def _decode_enum_by_index(self) -> HaxeEnum:
        # Wire format: "j" name ":" index ":" nargs (values...)
        name = self.decode_value()
        if not isinstance(name, str):
            raise HaxeDecodeError("Enum name was not a string")
        if self._peek() != ":":
            raise HaxeDecodeError("Expected ':' before enum index")
        self.pos += 1
        index = self._read_digits()
        if self._peek() != ":":
            raise HaxeDecodeError("Expected ':' before enum params count")
        self.pos += 1
        count = self._read_digits()
        result = HaxeEnum(name=name, constructor="", index=index)
        self.obj_cache.append(result)
        for _ in range(count):
            result.params.append(self.decode_value())
        return result


def decode(text: str) -> Any:
    """Decode a full Haxe-Serializer document into plain Python structures."""
    decoder = _Decoder(text)
    value = decoder.decode_value()
    return value


class _Encoder:
    def __init__(self):
        self.out: list[str] = []
        self.str_cache: dict[str, int] = {}

    def encode_value(self, value: Any) -> None:
        if value is None:
            self.out.append("n")
        elif isinstance(value, bool):
            self.out.append("t" if value else "f")
        elif isinstance(value, int):
            self.out.append("z" if value == 0 else f"i{value}")
        elif isinstance(value, float):
            if math.isnan(value):
                self.out.append("k")
            elif value == math.inf:
                self.out.append("p")
            elif value == -math.inf:
                self.out.append("m")
            else:
                self.out.append(f"d{value!r}")
        elif isinstance(value, str):
            self._encode_string(value)
        elif isinstance(value, HaxeList):
            self.out.append("l")
            for item in value:
                self.encode_value(item)
            self.out.append("h")
        elif isinstance(value, HaxeIntMap):
            self.out.append("q")
            for k, v in value.items():
                self.encode_value(k)
                self.encode_value(v)
            self.out.append("h")
        elif isinstance(value, HaxeStringMap):
            self.out.append("b")
            for k, v in value.items():
                self.encode_value(k)
                self.encode_value(v)
            self.out.append("h")
        elif isinstance(value, HaxeClassInstance):
            self.out.append("c")
            self._encode_string(value.name)
            for k, v in value.fields.items():
                self._encode_string(k)
                self.encode_value(v)
            self.out.append("g")
        elif isinstance(value, HaxeEnum):
            if value.index >= 0 and not value.constructor:
                self.out.append("j")
                self._encode_string(value.name)
                self.out.append(f":{value.index}:{len(value.params)}")
            else:
                self.out.append("w")
                self._encode_string(value.name)
                self._encode_string(value.constructor)
                self.out.append(f":{len(value.params)}")
            for p in value.params:
                self.encode_value(p)
        elif isinstance(value, list):
            self.out.append("a")
            for item in value:
                self.encode_value(item)
            self.out.append("h")
        elif isinstance(value, dict):
            self.out.append("o")
            for k, v in value.items():
                if not isinstance(k, str):
                    raise HaxeDecodeError(f"Anonymous object key must be a string, got {k!r}")
                self._encode_string(k)
                self.encode_value(v)
            self.out.append("g")
        else:
            raise HaxeDecodeError(f"Don't know how to encode Python value of type {type(value)!r}")

    def _encode_string(self, s: str) -> None:
        cached = self.str_cache.get(s)
        if cached is not None:
            self.out.append(f"R{cached}")
            return
        self.str_cache[s] = len(self.str_cache)
        encoded = _percent_encode(s)
        self.out.append(f"y{len(encoded)}:{encoded}")


def encode(value: Any) -> str:
    """Encode a plain Python structure (as produced by `decode`) back into Haxe-Serializer text.

    NOTE: this always rebuilds string/object cache indices from scratch in traversal
    order. That's a *valid* Haxe-Serializer document (Unserializer doesn't care what
    specific index numbers are used, only that back-references resolve correctly), but
    it will not be byte-identical to an original file that used different cache ordering
    or the "u" null-run array optimization. Verify with `decode(encode(x)) == x`-style
    round-tripping, not a raw byte diff against the original.
    """
    encoder = _Encoder()
    encoder.encode_value(value)
    return "".join(encoder.out)
