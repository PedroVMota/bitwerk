//! Hierarchy navigation and edit routing.
//!
//! The canvas shows either the root circuit or, after opening a macro
//! instance (double-click / "Edit Macro"), the body of its definition, in
//! place. Opening goes one level down per step; the breadcrumb bar shows
//! `root › Counter#3 › …` with "Up one level".
//!
//! * Edits made inside apply to the **definition**: the edit is applied to a
//!   working copy of the body (patching the body's scene, so it is visible
//!   in the same frame), then the whole definition is stored with
//!   [`Edit::DefineMacro`] on the root, which updates every instance (and the
//!   simulator, which recompiles off the UI thread). If the definition is
//!   refused (a cycle, a size limit) the body edit is dropped.
//! * The root's render scene is kept (and patched by every root-level
//!   change, e.g. instances that changed shape) while inside, so going back
//!   up is instant.
//! * The simulator is told which instance path is shown
//!   ([`bw_sim::Command::SetView`]), so while running the inside view shows
//!   the live nets of *that* instance.

use std::sync::Arc;

use bw_core::{Applied, Circuit, CircuitError, ComponentId, ComponentKind, Edit, MacroId};
use bw_render::{Camera, Scene};
use bw_sim::Command;

use crate::app::BitwerkApp;

/// One level below the root.
pub struct Level {
    pub def: MacroId,
    /// The instance opened (in the level above); `None` when a definition
    /// is edited on its own (no live state).
    pub inst: Option<ComponentId>,
    /// Working copy of the definition's body.
    pub body: Arc<Circuit>,
    /// Scene and camera of the level above, restored when going up.
    pub parent_scene: Box<Scene>,
    pub parent_camera: Camera,
}

impl BitwerkApp {
    /// The circuit on the canvas: the root, or the body being edited.
    #[must_use]
    pub fn shown(&self) -> &Circuit {
        self.levels.last().map_or(&*self.circuit, |l| &*l.body)
    }

    /// Instance path of the shown level (`Some([])` at the root), or `None`
    /// when a definition is open without an instance.
    #[must_use]
    pub fn view_path(&self) -> Option<Vec<ComponentId>> {
        self.levels.iter().map(|l| l.inst).collect()
    }

    /// Definition being edited, if inside one.
    #[must_use]
    pub fn current_def(&self) -> Option<MacroId> {
        self.levels.last().map(|l| l.def)
    }

    /// Breadcrumb labels: `root`, then `Name#id` per level.
    #[must_use]
    pub fn breadcrumbs(&self) -> Vec<String> {
        let mut v = vec!["root".to_string()];
        for l in &self.levels {
            let name = self.circuit.macros().def(l.def).map_or("?", |d| d.name.as_str()).to_string();
            v.push(match l.inst {
                Some(i) => format!("{name}#{}", i.0),
                None => format!("{name} (definition)"),
            });
        }
        v
    }

    fn send_view(&self) {
        // A path that cannot exist makes the simulator report "no view".
        let path = self.view_path().unwrap_or_else(|| vec![ComponentId(u32::MAX)]);
        self.sim.send(Command::SetView(path));
    }

    fn push_level(&mut self, def: MacroId, inst: Option<ComponentId>) {
        let Some(d) = self.circuit.macros().def(def) else { return };
        let body = Arc::new(d.body.clone());
        let scene = Box::new(Scene::build(&body, self.revision));
        let parent_scene = std::mem::replace(&mut self.scene, scene);
        let parent_camera = self.camera;
        self.levels.push(Level { def, inst, body, parent_scene, parent_camera });
        self.clear_selection();
        self.wire_draft = None;
        self.drag = None;
        self.hover_net = None;
        self.fit_pending = true;
        self.send_view();
        let n = self.circuit.total_instances(def);
        self.status = format!(
            "Editing macro '{}': changes apply to the definition ({n} instance{} in this design)",
            self.circuit.macros().def(def).map_or("?", |d| d.name.as_str()),
            if n == 1 { "" } else { "s" }
        );
    }

    /// Opens a macro instance of the shown circuit.
    pub fn enter_instance(&mut self, id: ComponentId) {
        if let Some(ComponentKind::Macro(s)) = self.shown().component(id).map(|c| c.kind) {
            self.push_level(s.def, Some(id));
        }
    }

    /// Opens a definition on its own (no instance, no live state).
    pub fn enter_def(&mut self, def: MacroId) {
        self.push_level(def, None);
    }

    /// Goes up until `depth` levels remain (0 = root).
    pub fn go_to_depth(&mut self, depth: usize) {
        while self.levels.len() > depth {
            let Some(l) = self.levels.pop() else { break };
            let mut scene = l.parent_scene;
            // A parent body may have changed while we were inside (its
            // instances of the edited macro were re-shaped): refresh it.
            if let Some(parent) = self.levels.last_mut() {
                if let Some(d) = self.circuit.macros().def(parent.def) {
                    if !d.body.content_eq(&parent.body) {
                        parent.body = Arc::new(d.body.clone());
                        scene = Box::new(Scene::build(&parent.body, self.revision));
                    }
                }
            }
            let old = std::mem::replace(&mut self.scene, scene);
            self.scene_worker.retire(old);
            self.camera = l.parent_camera;
        }
        self.clear_selection();
        self.wire_draft = None;
        self.drag = None;
        self.hover_net = None;
        self.send_view();
        self.status = if self.levels.is_empty() { "Root circuit".into() } else { "Up one level".into() };
    }

    pub fn go_up(&mut self) {
        if !self.levels.is_empty() {
            self.go_to_depth(self.levels.len() - 1);
        }
    }

    /// Leaves every level (the root circuit is being replaced).
    pub(crate) fn drop_levels(&mut self) {
        while let Some(l) = self.levels.pop() {
            let old = std::mem::replace(&mut self.scene, l.parent_scene);
            self.scene_worker.retire(old);
            self.camera = l.parent_camera;
        }
        self.send_view();
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
        self.selected_segment = None;
        self.multi.clear();
    }

    /// The root circuit's render scene (stashed while inside a macro).
    pub(crate) fn root_scene_mut(&mut self) -> &mut Box<Scene> {
        match self.levels.first_mut() {
            Some(l) => &mut l.parent_scene,
            None => &mut self.scene,
        }
    }

    /// Applies an edit to the **root** circuit (definitions, or ordinary
    /// edits at the root): UI copy, simulator copy and the root scene.
    ///
    /// # Errors
    /// The model rejected the edit; nothing changed.
    pub fn apply_root_edit(&mut self, edit: Edit) -> Result<Applied, CircuitError> {
        let applied = self.circuit_mut().apply(&edit)?;
        self.revision += 1;
        let revision = self.revision;
        self.sim.send(Command::Edit { edit, revision });
        if self.full_build_pending {
            self.scene_worker.request(Arc::clone(&self.circuit), revision);
        } else {
            let circuit = Arc::clone(&self.circuit);
            let scene = self.root_scene_mut();
            scene.apply(&circuit, &applied, revision);
            if self.levels.is_empty() && self.scene.free_ratio() > 0.5 && self.scene.slot_count() > 50_000 {
                self.scene_worker.request(Arc::clone(&self.circuit), self.revision);
                self.full_build_pending = true;
            }
        }
        if !self.levels.is_empty() {
            self.scene.set_revision(revision);
        }
        self.mark_dirty();
        Ok(applied)
    }

    /// Applies an edit to the body of the definition being edited and
    /// stores the definition (all instances follow).
    pub(crate) fn apply_body_edit(&mut self, edit: &Edit) -> Result<Applied, CircuitError> {
        let Some(level) = self.levels.last() else { return Err(CircuitError::NotRoot) };
        let def_id = level.def;
        let mut body = (*level.body).clone();
        let applied = body.apply(edit)?;
        let mut def = (**self.circuit.macros().def(def_id).ok_or(CircuitError::NoSuchMacro(def_id))?).clone();
        def.body = body.clone();
        def.sync_pins();
        self.apply_root_edit(Edit::DefineMacro { id: def_id, def: Arc::new(def) })?;
        if let Some(level) = self.levels.last_mut() {
            level.body = Arc::new(body);
            let revision = self.revision;
            self.scene.apply(&self.levels[self.levels.len() - 1].body, &applied, revision);
        }
        Ok(applied)
    }

    /// Stores a changed definition (properties editor, library update).
    ///
    /// # Errors
    /// Refused definition.
    pub fn redefine(&mut self, id: MacroId, def: bw_core::MacroDef) -> Result<Applied, CircuitError> {
        let a = self.apply_root_edit(Edit::DefineMacro { id, def: Arc::new(def) })?;
        // An open level showing this definition follows.
        let last = self.levels.len().saturating_sub(1);
        let mut rebuild = false;
        for (i, l) in self.levels.iter_mut().enumerate() {
            if l.def != id {
                continue;
            }
            if let Some(d) = self.circuit.macros().def(id) {
                if !d.body.content_eq(&l.body) {
                    l.body = Arc::new(d.body.clone());
                    rebuild |= i == last;
                }
            }
        }
        if rebuild {
            let scene = Box::new(Scene::build(&self.levels[last].body, self.revision));
            let old = std::mem::replace(&mut self.scene, scene);
            self.scene_worker.retire(old);
        }
        Ok(a)
    }
}
