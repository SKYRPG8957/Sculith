package dev.sculith.companion;

import dev.sculith.companion.mixin.DialogAccess;
import net.fabricmc.api.ClientModInitializer;
import net.fabricmc.fabric.api.client.screen.v1.ScreenEvents;
import net.fabricmc.fabric.api.client.screen.v1.Screens;
import net.minecraft.client.Minecraft;
import net.minecraft.client.gui.GuiGraphicsExtractor;
import net.minecraft.client.gui.components.AbstractButton;
import net.minecraft.client.gui.components.AbstractWidget;
import net.minecraft.client.gui.narration.NarrationElementOutput;
import net.minecraft.client.gui.screens.Screen;
import net.minecraft.client.gui.screens.dialog.DialogControlSet;
import net.minecraft.client.gui.screens.dialog.DialogScreen;
import net.minecraft.client.input.InputWithModifiers;
import net.minecraft.client.renderer.RenderPipelines;
import net.minecraft.network.chat.ClickEvent;
import net.minecraft.network.chat.Component;
import net.minecraft.server.dialog.MultiActionDialog;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

import java.io.IOException;
import java.util.ArrayList;
import java.util.List;
import java.util.Optional;
import java.util.WeakHashMap;

public final class Companion implements ClientModInitializer {
    private static final Logger LOG = LoggerFactory.getLogger("Sculith Companion");
    private final WeakHashMap<Screen, BedrockLayout> active = new WeakHashMap<>();

    @Override
    public void onInitializeClient() {
        ScreenEvents.AFTER_INIT.register(this::install);
    }

    private void install(Minecraft game, Screen screen, int width, int height) {
        if (!(screen instanceof DialogScreen<?> dialog)
                || !(((DialogAccess) dialog).sculith$getDialog() instanceof MultiActionDialog menu)
                || !menu.common().inputs().isEmpty() || menu.actions().size() > 64) return;
        var binder = new DialogControlSet(dialog);
        List<ClickEvent> events = new ArrayList<>();
        String token = null;
        for (int i = 0; i < menu.actions().size(); i++) {
            var click = binder.bindAction(menu.actions().get(i).action()).get();
            if (click.isEmpty() || !(click.get() instanceof ClickEvent.Custom custom)) return;
            String id = custom.id().toString();
            if (!id.matches("sculith:ui/[0-9]{1,10}/[0-9a-f]{32}/" + i)) return;
            String prefix = id.substring(0, id.lastIndexOf('/'));
            if (token != null && !token.equals(prefix)) return;
            token = prefix;
            events.add(custom);
        }
        if (menu.exitAction().isEmpty() || token == null) return;
        var exit = binder.bindAction(menu.exitAction().get().action()).get();
        if (exit.isEmpty() || !(exit.get() instanceof ClickEvent.Custom close)
                || !close.id().toString().equals(token + "/cancel")) return;
        List<String> labels = menu.actions().stream().map(action -> action.button().label().getString()).toList();
        BedrockLayout layout;
        try {
            layout = BedrockLayout.read(game.getResourceManager(), screen.getTitle().getString(), labels);
        } catch (IOException | RuntimeException error) {
            LOG.warn("Cannot display the Bedrock action form", error);
            return;
        }
        if (layout == null) return;
        List<AbstractWidget> widgets = Screens.getWidgets(screen);
        widgets.clear();
        for (int i = 0; i < events.size(); i++) {
            var button = new ImageButton(screen, layout.size(), layout.buttons().get(i), menu.actions().get(i).button().label(),
                    dialog, events.get(i));
            button.arrange();
            widgets.add(button);
        }
        if (active.put(screen, layout) == null) {
            ScreenEvents.afterBackground(screen).register((current, graphics, mouseX, mouseY, tick) -> {
                var latest = active.get(current);
                if (latest == null) return;
                var area = new Area(current, latest.size());
                for (var image : latest.pictures()) area.blit(graphics, image);
            });
        }
        LOG.debug("Opened Bedrock JSON UI {}", screen.getTitle().getString());
    }

    /** Maps both rendering and hit testing from the same Bedrock GUI coordinates. */
    private record Area(int left, int top, float scale) {
        Area(Screen screen, BedrockLayout.Size canvas) {
            this(screen, canvas, Math.min(1f, Math.min((float)screen.width / canvas.w(), (float)screen.height / canvas.h())));
        }
        Area(Screen screen, BedrockLayout.Size canvas, float scale) {
            this((screen.width - Math.round(canvas.w() * scale)) / 2,
                    (screen.height - Math.round(canvas.h() * scale)) / 2, scale);
        }
        int x(int x) { return left + Math.round(scale * x); }
        int y(int y) { return top + Math.round(scale * y); }
        void blit(GuiGraphicsExtractor graphics, BedrockLayout.Picture picture) {
            var box = picture.box();
            int w = box.size().w(), h = box.size().h();
            graphics.blit(RenderPipelines.GUI_TEXTURED, picture.texture(), x(box.x()), y(box.y()), 0f, 0f,
                    x(box.x() + w) - x(box.x()), y(box.y() + h) - y(box.y()), w, h, w, h);
        }
    }

    private static final class ImageButton extends AbstractButton {
        private final Screen screen;
        private final BedrockLayout.Size canvas;
        private final BedrockLayout.ButtonArt art;
        private final DialogScreen<?> dialog;
        private final ClickEvent event;
        private long pressedUntil;

        ImageButton(Screen screen, BedrockLayout.Size canvas, BedrockLayout.ButtonArt art,
                    Component label, DialogScreen<?> dialog, ClickEvent event) {
            super(0, 0, 1, 1, label);
            this.screen = screen;
            this.canvas = canvas;
            this.art = art;
            this.dialog = dialog;
            this.event = event;
        }
        void arrange() {
            var area = new Area(screen, canvas);
            var box = art.box();
            setRectangle(area.x(box.x() + box.size().w()) - area.x(box.x()),
                    area.y(box.y() + box.size().h()) - area.y(box.y()), area.x(box.x()), area.y(box.y()));
        }
        @Override
        protected void extractContents(GuiGraphicsExtractor graphics, int mouseX, int mouseY, float tick) {
            arrange();
            boolean pointed = mouseX >= getX() && mouseY >= getY()
                    && mouseX < getX() + getWidth() && mouseY < getY() + getHeight();
            var image = System.nanoTime() < pressedUntil ? art.pressed() : pointed || isHoveredOrFocused() ? art.hover() : art.normal();
            new Area(screen, canvas).blit(graphics, image);
        }
        @Override
        public void onPress(InputWithModifiers input) {
            pressedUntil = System.nanoTime() + 100_000_000L;
            dialog.runAction(Optional.of(event));
        }
        @Override
        protected void updateWidgetNarration(NarrationElementOutput output) {
            defaultButtonNarrationText(output);
        }
    }
}
