package dev.sculith.companion;

import com.google.gson.JsonArray;
import com.google.gson.JsonElement;
import com.google.gson.JsonObject;
import com.google.gson.JsonParser;
import net.minecraft.resources.Identifier;
import net.minecraft.server.packs.resources.ResourceManager;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;

/** Reads the centered, fixed-pixel panel and native action-form collection from the delivered Bedrock UI. */
record BedrockLayout(Size size, List<Picture> pictures, List<ButtonArt> buttons) {
    private static final Identifier FORM = Identifier.fromNamespaceAndPath("sculith", "bedrock_ui/server_form.json");

    record Size(int w, int h) {
        static Size read(JsonObject object) {
            JsonArray pair = object.getAsJsonArray("size");
            int w = pair.get(0).getAsInt(), h = pair.get(1).getAsInt();
            if (pair.size() != 2 || w < 1 || h < 1 || w > 2048 || h > 2048) throw new IllegalArgumentException("Invalid control size");
            return new Size(w, h);
        }
    }
    record Box(int x, int y, Size size) {
        static Box at(JsonObject object, int parentX, int parentY) {
            if (!"top_left".equals(object.get("anchor_from").getAsString())
                    || !"top_left".equals(object.get("anchor_to").getAsString())) throw new IllegalArgumentException("Unsupported control anchor");
            JsonArray xy = object.getAsJsonArray("offset");
            return new Box(parentX + xy.get(0).getAsInt(), parentY + xy.get(1).getAsInt(), Size.read(object));
        }
    }
    record Picture(Identifier texture, Box box) {}
    record ButtonArt(Box box, Picture normal, Picture hover, Picture pressed) {}

    static BedrockLayout read(ResourceManager resources, String title, List<String> labels) throws IOException {
        var source = resources.getResource(FORM);
        if (source.isEmpty()) return null;
        JsonObject ui;
        try (var stream = source.get().open()) {
            var bytes = stream.readNBytes(131073);
            if (bytes.length > 131072) throw new IllegalArgumentException("Form JSON exceeds 128 KiB");
            ui = JsonParser.parseString(new String(bytes, StandardCharsets.UTF_8)).getAsJsonObject();
        }
        if (!"server_form".equals(ui.get("namespace").getAsString())) return null;
        for (JsonElement mod : ui.getAsJsonObject("main_screen_content").getAsJsonArray("modifications")) {
            if (!"controls".equals(mod.getAsJsonObject().get("array_name").getAsString())) continue;
            for (JsonElement control : mod.getAsJsonObject().getAsJsonArray("value")) {
                JsonObject factory = only(control).getValue().getAsJsonObject().getAsJsonObject("factory");
                if (factory == null || !"server_form_factory".equals(factory.get("name").getAsString())) continue;
                String ref = factory.getAsJsonObject("control_ids").get("long_form").getAsString();
                JsonObject root = resolve(ui, ref);
                if (!bound(root, "#title_text", title)) continue;
                if (!"center".equals(root.get("anchor_from").getAsString()) || !"center".equals(root.get("anchor_to").getAsString())) throw new IllegalArgumentException("Expected a centered form");
                List<Picture> images = new ArrayList<>();
                List<ButtonArt> buttons = new ArrayList<>();
                for (JsonElement item : controls(root)) {
                    JsonObject widget = only(item).getValue().getAsJsonObject();
                    switch (widget.get("type").getAsString()) {
                        case "image" -> images.add(picture(widget, 0, 0, resources));
                        case "stack_panel" -> buttons.addAll(collection(ui, widget, labels, resources));
                        default -> throw new IllegalArgumentException("Unsupported Bedrock panel child");
                    }
                }
                if (buttons.size() != labels.size()) throw new IllegalArgumentException("Form action count differs");
                return new BedrockLayout(Size.read(root), List.copyOf(images), List.copyOf(buttons));
            }
        }
        return null;
    }

    private static List<ButtonArt> collection(JsonObject ui, JsonObject panel, List<String> labels, ResourceManager resources) {
        if (!"form_buttons".equals(panel.get("collection_name").getAsString())) throw new IllegalArgumentException("Expected form_buttons");
        String orientation = panel.get("orientation").getAsString();
        if (!List.of("horizontal","vertical").contains(orientation)) throw new IllegalArgumentException("Unknown stack direction");
        Box area = Box.at(panel, 0, 0);
        JsonObject repeated = resolve(ui, panel.getAsJsonObject("factory").getAsJsonObject("control_ids").get("button").getAsString());
        Size cell = Size.read(repeated);
        var hitEntry = only(controls(repeated).get(0));
        if (!hitEntry.getKey().endsWith("@common.button")) throw new IllegalArgumentException("Expected a native Bedrock button");
        JsonObject hit = hitEntry.getValue().getAsJsonObject();
        if (!"button.form_button_click".equals(hit.get("$pressed_button_name").getAsString())) throw new IllegalArgumentException("Unknown action binding");
        List<ButtonArt> buttons = new ArrayList<>();
        for (int i=0; i<labels.size(); i++) {
            int x = area.x() + ("horizontal".equals(orientation) ? i * cell.w() : 0);
            int y = area.y() + ("vertical".equals(orientation) ? i * cell.h() : 0);
            Box box = Box.at(hit, x, y);
            String label = labels.get(i);
            buttons.add(new ButtonArt(box, state(hit, "default", label, box, resources),
                state(hit, "hover", label, box, resources), state(hit, "pressed", label, box, resources)));
        }
        return buttons;
    }

    private static Picture state(JsonObject hit, String state, String label, Box box, ResourceManager resources) {
        for (JsonElement wrapper : controls(hit)) {
            var entry = only(wrapper);
            if (!entry.getKey().equals(state)) continue;
            for (JsonElement option : controls(entry.getValue().getAsJsonObject())) {
                JsonObject image = only(option).getValue().getAsJsonObject();
                if (bound(image, "#form_button_text", label)) return picture(image, box.x(), box.y(), resources);
            }
        }
        throw new IllegalArgumentException("Missing button art for " + label);
    }

    private static boolean bound(JsonObject object, String variable, String value) {
        if (!object.has("bindings")) return false;
        for (JsonElement entry : object.getAsJsonArray("bindings")) {
            JsonObject binding = entry.getAsJsonObject();
            if (binding.has("target_property_name") && "#visible".equals(binding.get("target_property_name").getAsString())
                    && ("(" + variable + " = '" + value + "')").equals(binding.get("source_property_name").getAsString())) return true;
        }
        return false;
    }

    private static Picture picture(JsonObject node, int x, int y, ResourceManager resources) {
        String name = node.get("texture").getAsString();
        if (!name.matches("textures/[a-z0-9_/]+")) throw new IllegalArgumentException("Invalid Bedrock texture path");
        var texture = Identifier.fromNamespaceAndPath("sculith", name + ".png");
        if (resources.getResource(texture).isEmpty()) throw new IllegalArgumentException("Missing texture " + name);
        return new Picture(texture, Box.at(node, x, y));
    }

    private static JsonObject resolve(JsonObject ui, String reference) {
        String name = reference.startsWith("@") ? reference.substring(1) : reference;
        if (!name.startsWith("server_form.")) throw new IllegalArgumentException("Unknown control namespace");
        return ui.getAsJsonObject(name.substring("server_form.".length()));
    }

    private static JsonArray controls(JsonObject node) {
        JsonArray result = node.getAsJsonArray("controls");
        if (result.size() > 128) throw new IllegalArgumentException("Too many Bedrock controls");
        return result;
    }

    private static Map.Entry<String, JsonElement> only(JsonElement object) {
        var entries = object.getAsJsonObject().entrySet();
        if (entries.size() != 1) throw new IllegalArgumentException("Expected one named Bedrock control");
        return entries.iterator().next();
    }
}
