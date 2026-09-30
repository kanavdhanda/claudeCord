//! Who is allowed to do what. Humans are known by their chat account id and have a role per project. Owners of the
//! workspace are owners everywhere. Only an owner can change roles, and an unlisted account can do nothing at all.

use super::core::HubCore;
use super::model::*;

impl HubCore {
    /// Declares an account to be an owner of the whole workspace. Called at setup, from the verified login.
    pub fn add_owner(&mut self, id: &str) {
        self.owners.insert(id.to_string());
    }

    /// The role of an account in a project, or None if the account is not on the list.
    pub fn role_of(&self, project: &str, id: &str) -> Option<Role> {
        if self.owners.contains(id) {
            return Some(Role::Owner);
        }
        self.members
            .get(project)
            .and_then(|m| m.get(id))
            .map(|m| m.role)
    }

    /// Checks an account has at least `min` in a project. Returns its role, or says why not.
    pub(super) fn require(&self, project: &str, id: &str, min: Role) -> Result<Role, Denied> {
        match self.role_of(project, id) {
            None => Err(Denied::Unlisted),
            Some(r) if r >= min => Ok(r),
            Some(_) => Err(Denied::NeedsRole(min)),
        }
    }

    /// How a person is shown to agents: `name (role)`. Agents see who is speaking and how much weight it carries.
    pub(super) fn label(&self, project: &str, h: &Human) -> String {
        let role = self.role_of(project, &h.id).map_or("guest", Role::name);
        format!("{} ({role})", super::routing::clean_label(&h.name))
    }

    /// Sets or removes a person's role in a project. Only an owner may do this. `None` removes the person.
    pub fn set_role(
        &mut self,
        by: &Human,
        project: &str,
        target: &Human,
        role: Option<Role>,
    ) -> Result<(), Denied> {
        self.require(project, &by.id, Role::Owner)?;
        let list = self.members.entry(project.to_string()).or_default();
        match role {
            Some(role) => {
                list.insert(
                    target.id.clone(),
                    Member {
                        id: target.id.clone(),
                        name: target.name.clone(),
                        role,
                    },
                );
            }
            None => {
                list.remove(&target.id);
            }
        }
        Ok(())
    }

    /// The people on a project's list, for display. Workspace owners are not included, they are implicit.
    pub fn members_of(&self, project: &str) -> Vec<&Member> {
        self.members
            .get(project)
            .map(|m| m.values().collect())
            .unwrap_or_default()
    }
}
