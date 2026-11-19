"""Compile the guild's Bedrock image panel and horizontal action collection for Java 26.3."""
from collections import defaultdict
from io import BytesIO
import json
import re

from PIL import Image


FONT = "sculith:guild_vanilla"
LINE = 9
BUTTON_LABEL = re.compile(r"\(#form_button_text = '([^']+)'\)")


def child(control):
    (entry,) = control.items()
    return entry


def draw(canvas, files, control, origin):
    if control["type"] != "image" or control["anchor_from"] != "top_left" or control["anchor_to"] != "top_left":
        raise ValueError("Expected a top-left Bedrock image")
    image = Image.open(BytesIO(files[control["texture"] + ".png"])).convert("RGBA")
    size = tuple(control["size"])
    if image.size != size:
        image = image.resize(size, Image.Resampling.NEAREST)
    dx, dy = control["offset"]
    canvas.alpha_composite(image, (origin[0] + dx, origin[1] + dy))


def compose(files, labels):
    ui = json.loads(files["ui/server_form.json"])
    panel = ui["guild_panel"]
    canvas = Image.new("RGBA", tuple(panel["size"]))
    bounds = []
    for element in panel["controls"]:
        _, control = child(element)
        if control["type"] == "image":
            draw(canvas, files, control, (0, 0))
            continue
        if control["type"] != "stack_panel" or control["orientation"] != "horizontal" or control["collection_name"] != "form_buttons":
            raise ValueError("Expected a horizontal form_buttons collection")
        cell = ui[control["factory"]["control_ids"]["button"].split(".")[-1]]
        _, hit = child(cell["controls"][0])
        normal = next(item["default"] for item in hit["controls"] if "default" in item)
        by_label = {}
        for item in normal["controls"]:
            _, image = child(item)
            expression = next(b["source_property_name"] for b in image["bindings"] if b.get("target_property_name") == "#visible")
            match = BUTTON_LABEL.fullmatch(expression)
            if match is None:
                raise ValueError("Expected a button label visibility binding")
            by_label[match[1]] = image
        for index, label in enumerate(labels):
            x = control["offset"][0] + cell["size"][0] * index + hit["offset"][0]
            y = control["offset"][1] + hit["offset"][1]
            w, h = hit["size"]
            if not (0 <= x < x + w <= canvas.width and 0 <= y < y + h <= canvas.height):
                raise ValueError("Action extends beyond the authored panel")
            draw(canvas, files, by_label[label], (x, y))
            bounds.append((x, y, w, h))
    if len(bounds) != len(labels):
        raise ValueError("Expected one hit area per guild action")
    return canvas, bounds


def segments(width, bounds, top):
    cursor = 0
    for index, (x, y, w, h) in enumerate(bounds):
        if not y <= top < y + h:
            continue
        if x < cursor:
            raise ValueError("Overlapping guild action hit areas")
        if cursor < x:
            yield cursor, x, None
        yield x, x + w, index
        cursor = x + w
    if cursor < width:
        yield cursor, width, None


def atlas(width, tiles):
    rows = min(len(tiles), 16)
    cols = (len(tiles) + rows - 1) // rows
    page = Image.new("RGBA", (width * cols, rows * LINE))
    chars = [["\x00"] * cols for _ in range(rows)]
    for index, (code, tile) in enumerate(tiles):
        y, x = divmod(index, cols)
        page.paste(tile, (x * width, y * LINE))
        chars[y][x] = code
    data = BytesIO()
    page.save(data, format="PNG")
    name = f"ui/guild_vanilla/rows_{width}.png"
    provider = {"type": "bitmap", "file": "sculith:" + name, "ascent": 8, "height": LINE,
                "chars": ["".join(line) for line in chars]}
    return name, data.getvalue(), provider


def compile_form(files, labels):
    image, bounds = compose(files, labels)
    pad = -bounds[0][1] % LINE
    canvas = Image.new("RGBA", (image.width, ((image.height + pad + LINE - 1) // LINE) * LINE))
    canvas.paste(image, (0, pad))
    pages = defaultdict(list)
    advances = {}
    parts = []
    code = 0xe200
    for top in range(0, canvas.height, LINE):
        if top:
            parts.append({"text": "\n"})
        else:
            # Give Java's FocusableTextWidget a wider first line for its independent height measurement.
            parts.append({"text": "  "})
        for x, end, button in segments(image.width, bounds, top - pad):
            glyphs = []
            while x < end:
                width = min(80, end - x)
                tile = canvas.crop((x, top, x + width, top + LINE))
                box = tile.getchannel("A").getbbox()
                advance = width - (box[2] if box else 0) - 1
                space = chr(0xe101 + advance)
                advances[space] = advance
                if code > 0xf8ff:
                    raise ValueError("Too many glyphs for the form font")
                pages[width].append((chr(code), tile))
                glyphs.extend((chr(code), space))
                code += 1
                x += width
            part = {"text": "".join(glyphs), "font": FONT, "shadow_color": 0}
            if button is not None:
                part["click_event"] = {"action": "custom", "id": f"sculith:guild/REQUEST/{button}"}
            parts.append(part)
        if not top:
            parts.append({"text": "  "})
    providers = [{"type": "space", "advances": advances}]
    java = {}
    for width, tiles in pages.items():
        name, png, provider = atlas(width, tiles)
        java["assets/sculith/textures/" + name] = png
        providers.append(provider)
    java["assets/sculith/font/guild_vanilla.json"] = json.dumps({"providers": providers}, ensure_ascii=False).encode()
    body = {"type": "minecraft:plain_message", "width": image.width + 30, "contents": {"text": "", "extra": parts}}
    dialog = {"type": "minecraft:dialog_list", "title": {"text": ""}, "external_title": {"text": "길드 의뢰"},
              "pause": False, "can_close_with_escape": True, "after_action": "close", "dialogs": [], "body": [body]}
    return java, json.dumps(dialog, ensure_ascii=False, separators=(",", ":"))
