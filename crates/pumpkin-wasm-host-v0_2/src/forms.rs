use crate::pumpkin::plugin::forms::{
    CustomForm, CustomFormElement, Host, ImageType, ModalForm, SimpleForm,
};
use pumpkin_wasm_host_common::state::PluginHostState;
use sculith_core::entity::player::forms::JavaFormFonts;
use sculith_util::translation::Locale;
use serde_json::{Value, json};
use wasmtime::component::Resource;

impl Host for PluginHostState {}

pub trait FormsHostStateExt {
    fn translate_res_v0_2(
        &mut self,
        res: Resource<crate::pumpkin::plugin::text::TextComponent>,
        locale: Locale,
    ) -> (String, Option<String>);
    fn serialize_simple_form_v0_2(
        &mut self,
        form: SimpleForm,
        locale: Locale,
    ) -> (Value, JavaFormFonts);
    fn serialize_modal_form_v0_2(
        &mut self,
        form: ModalForm,
        locale: Locale,
    ) -> (Value, JavaFormFonts);
    fn serialize_custom_form_v0_2(
        &mut self,
        form: CustomForm,
        locale: Locale,
    ) -> (Value, JavaFormFonts);
}

impl FormsHostStateExt for PluginHostState {
    fn translate_res_v0_2(
        &mut self,
        res: Resource<crate::pumpkin::plugin::text::TextComponent>,
        locale: Locale,
    ) -> (String, Option<String>) {
        let component = self.take(res).expect("Couldn't get text component");
        let font = component.0.style.font.clone();
        (component.0.get_text(locale), font)
    }

    fn serialize_simple_form_v0_2(
        &mut self,
        form: SimpleForm,
        locale: Locale,
    ) -> (Value, JavaFormFonts) {
        let (title, title_font) = self.translate_res_v0_2(form.title, locale);
        let (content, body_font) = self.translate_res_v0_2(form.content, locale);
        let mut controls = Vec::with_capacity(form.buttons.len());
        let buttons: Vec<Value> = form
            .buttons
            .into_iter()
            .map(|b| {
                let (text, font) = self.translate_res_v0_2(b.text, locale);
                controls.push(font);
                let mut obj = json!({ "text": text });
                if let Some(image) = b.image
                    && let Some(obj) = obj.as_object_mut()
                {
                    obj.insert(
                        "image".to_string(),
                        json!({
                            "type": match image.type_ {
                                ImageType::Url => "url",
                                ImageType::Path => "path",
                            },
                            "data": image.data
                        }),
                    );
                }
                obj
            })
            .collect();

        (
            json!({ "type": "form", "title": title, "content": content, "buttons": buttons }),
            JavaFormFonts {
                title: title_font,
                body: body_font,
                controls,
            },
        )
    }

    fn serialize_modal_form_v0_2(
        &mut self,
        form: ModalForm,
        locale: Locale,
    ) -> (Value, JavaFormFonts) {
        let (title, title_font) = self.translate_res_v0_2(form.title, locale);
        let (content, body_font) = self.translate_res_v0_2(form.content, locale);
        let (button1, first_font) = self.translate_res_v0_2(form.button1, locale);
        let (button2, second_font) = self.translate_res_v0_2(form.button2, locale);
        (
            json!({ "type": "modal", "title": title, "content": content, "button1": button1, "button2": button2 }),
            JavaFormFonts {
                title: title_font,
                body: body_font,
                controls: vec![first_font, second_font],
            },
        )
    }

    fn serialize_custom_form_v0_2(
        &mut self,
        form: CustomForm,
        locale: Locale,
    ) -> (Value, JavaFormFonts) {
        let (title, title_font) = self.translate_res_v0_2(form.title, locale);
        let mut controls = Vec::with_capacity(form.elements.len());
        let mut label = |resource| {
            let (text, font) = self.translate_res_v0_2(resource, locale);
            controls.push(font);
            text
        };
        let elements: Vec<Value> = form.elements.into_iter().map(|e| {
            match e {
                CustomFormElement::Label(text) => json!({ "type": "label", "text": label(text) }),
                CustomFormElement::Toggle((text, default)) => json!({ "type": "toggle", "text": label(text), "default": default }),
                CustomFormElement::Slider((text, min, max, step, default)) => json!({
                    "type": "slider", "text": label(text), "min": min, "max": max, "step": step, "default": default
                }),
                CustomFormElement::StepSlider((text, steps, default)) => json!({
                    "type": "step_slider", "text": label(text), "steps": steps, "default": default
                }),
                CustomFormElement::Dropdown((text, options, default)) => json!({
                    "type": "dropdown", "text": label(text), "options": options, "default": default
                }),
                CustomFormElement::Input((text, placeholder, default)) => json!({
                    "type": "input", "text": label(text), "placeholder": placeholder, "default": default
                }),
            }
        }).collect();

        (
            json!({ "type": "custom_form", "title": title, "content": elements }),
            JavaFormFonts {
                title: title_font,
                body: None,
                controls,
            },
        )
    }
}
