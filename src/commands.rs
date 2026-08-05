//! # Default Commands
//!
//! The command-bar commands are set up here, and iamb-specific commands are defined here. See
//! [modalkit::env::vim::command] for additional Vim commands we pull in.
use std::{convert::TryFrom, str::FromStr as _};

use matrix_sdk::ruma::{events::tag::TagName, OwnedRoomId, OwnedUserId};

use modalkit::{
    commands::{CommandError, CommandResult, CommandStep},
    env::vim::command::{CommandContext, CommandDescription, CommandFunc, OptionType},
    prelude::OpenTarget,
};

use crate::base::{
    CreateRoomFlags,
    CreateRoomType,
    DownloadFlags,
    HomeserverAction,
    IambAction,
    IambId,
    IambInfo,
    KeysAction,
    MemberUpdateAction,
    MessageAction,
    ProgramCommand,
    ProgramCommands,
    RoomAction,
    RoomField,
    SendAction,
    SpaceAction,
    VerifyAction,
};

type ProgContext = CommandContext;
type ProgResult = CommandResult<ProgramCommand>;

/// Convert strings the user types into a tag name.
fn tag_name(name: String) -> Result<TagName, CommandError> {
    let tag = match name.to_lowercase().as_str() {
        "fav" | "favorite" | "favourite" | "m.favourite" => TagName::Favorite,
        "low" | "lowpriority" | "low_priority" | "low-priority" | "m.lowpriority" => {
            TagName::LowPriority
        },
        "servernotice" | "server_notice" | "server-notice" | "m.server_notice" => {
            TagName::ServerNotice
        },
        _ => {
            if let Ok(tag) = name.parse() {
                TagName::User(tag)
            } else {
                let msg = format!("Invalid user tag name: {name}");

                return Err(CommandError::Error(msg));
            }
        },
    };

    Ok(tag)
}

fn iamb_invite(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    let args = desc.arg.strings()?;

    if args.is_empty() {
        return Err(CommandError::InvalidArgument);
    }

    let ract = match args[0].as_str() {
        "accept" => {
            if args.len() != 1 {
                return Err(CommandError::InvalidArgument);
            }

            RoomAction::InviteAccept
        },
        "reject" => {
            if args.len() != 1 {
                return Err(CommandError::InvalidArgument);
            }

            RoomAction::InviteReject
        },
        "send" => {
            if args.len() != 2 {
                return Err(CommandError::InvalidArgument);
            }

            if let Ok(user) = OwnedUserId::try_from(args[1].as_str()) {
                RoomAction::InviteSend(user)
            } else {
                let msg = format!("Invalid user identifier: {}", args[1]);
                let err = CommandError::Error(msg);

                return Err(err);
            }
        },
        _ => {
            return Err(CommandError::InvalidArgument);
        },
    };

    let iact = IambAction::from(ract);
    let step = CommandStep::Continue(iact.into(), ctx.context.clone());

    return Ok(step);
}

fn iamb_keys(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    let mut args = desc.arg.strings()?;

    if args.len() != 3 {
        return Err(CommandError::InvalidArgument);
    }

    let act = args.remove(0);
    let path = args.remove(0);
    let passphrase = args.remove(0);

    let act = match act.as_str() {
        "export" => KeysAction::Export(path, passphrase),
        "import" => KeysAction::Import(path, passphrase),
        _ => return Err(CommandError::InvalidArgument),
    };

    let vact = IambAction::Keys(act);
    let step = CommandStep::Continue(vact.into(), ctx.context.clone());

    return Ok(step);
}

fn iamb_verify(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    let mut args = desc.arg.strings()?;

    match args.len() {
        0 => {
            let open = ctx.switch(OpenTarget::Application(IambId::VerifyList));
            let step = CommandStep::Continue(open, ctx.context.clone());

            return Ok(step);
        },
        1 => {
            return Result::Err(CommandError::InvalidArgument);
        },
        2 => {
            let act = match args[0].as_str() {
                "accept" => VerifyAction::Accept,
                "cancel" => VerifyAction::Cancel,
                "confirm" => VerifyAction::Confirm,
                "mismatch" => VerifyAction::Mismatch,
                "request" => {
                    let iact = IambAction::VerifyRequest(args.remove(1));
                    let step = CommandStep::Continue(iact.into(), ctx.context.clone());

                    return Ok(step);
                },
                _ => return Result::Err(CommandError::InvalidArgument),
            };

            let vact = IambAction::Verify(act, args.remove(1));
            let step = CommandStep::Continue(vact.into(), ctx.context.clone());

            return Ok(step);
        },
        _ => {
            return Result::Err(CommandError::InvalidArgument);
        },
    }
}

fn iamb_dms(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    if !desc.arg.text.is_empty() {
        return Result::Err(CommandError::InvalidArgument);
    }

    let open = ctx.switch(OpenTarget::Application(IambId::DirectList));
    let step = CommandStep::Continue(open, ctx.context.clone());

    return Ok(step);
}

fn iamb_members(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    if !desc.arg.text.is_empty() {
        return Result::Err(CommandError::InvalidArgument);
    }

    let open = IambAction::Room(RoomAction::Members(ctx.clone().into()));
    let step = CommandStep::Continue(open.into(), ctx.context.clone());

    return Ok(step);
}

fn iamb_leave(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    if !desc.arg.text.is_empty() {
        return Result::Err(CommandError::InvalidArgument);
    }

    let leave = IambAction::Room(RoomAction::Leave(desc.bang));
    let step = CommandStep::Continue(leave.into(), ctx.context.clone());

    return Ok(step);
}

fn iamb_forget(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    if !desc.arg.text.is_empty() {
        return Result::Err(CommandError::InvalidArgument);
    }

    let forget = IambAction::Homeserver(HomeserverAction::Forget);
    let step = CommandStep::Continue(forget.into(), ctx.context.clone());

    return Ok(step);
}

fn iamb_cancel(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    if !desc.arg.text.is_empty() {
        return Result::Err(CommandError::InvalidArgument);
    }

    let mact = IambAction::from(MessageAction::Cancel(desc.bang));
    let step = CommandStep::Continue(mact.into(), ctx.context.clone());

    return Ok(step);
}

fn iamb_edit(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    if !desc.arg.text.is_empty() {
        return Result::Err(CommandError::InvalidArgument);
    }

    let mact = IambAction::from(MessageAction::Edit);
    let step = CommandStep::Continue(mact.into(), ctx.context.clone());

    return Ok(step);
}

fn iamb_react(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    let mut args = desc.arg.strings()?;

    if args.len() != 1 {
        return Result::Err(CommandError::InvalidArgument);
    }

    let react = args.remove(0);
    let mact = IambAction::from(MessageAction::React(react, desc.bang));
    let step = CommandStep::Continue(mact.into(), ctx.context.clone());

    return Ok(step);
}

fn iamb_unreact(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    let mut args = desc.arg.strings()?;

    if args.len() > 1 {
        return Result::Err(CommandError::InvalidArgument);
    }

    let reaction = args.pop();
    let mact = IambAction::from(MessageAction::Unreact(reaction, desc.bang));
    let step = CommandStep::Continue(mact.into(), ctx.context.clone());

    return Ok(step);
}

fn iamb_redact(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    let args = desc.arg.strings()?;

    if args.len() > 1 {
        return Result::Err(CommandError::InvalidArgument);
    }

    let reason = args.into_iter().next();
    let ract = IambAction::from(MessageAction::Redact(reason, desc.bang));
    let step = CommandStep::Continue(ract.into(), ctx.context.clone());

    return Ok(step);
}

fn iamb_pipe(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    // The command is taken as it was written, and handed to a shell whole. Splitting it here
    // would lose the pipelines and the redirections that make the command worth having.
    let cmd = desc.arg.text.trim().to_string();

    if cmd.is_empty() {
        return Result::Err(CommandError::InvalidArgument);
    }

    let ract = IambAction::from(MessageAction::Pipe(cmd));
    let step = CommandStep::Continue(ract.into(), ctx.context.clone());

    return Ok(step);
}

fn iamb_reply(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    if !desc.arg.text.is_empty() {
        return Result::Err(CommandError::InvalidArgument);
    }

    let ract = IambAction::from(MessageAction::Reply);
    let step = CommandStep::Continue(ract.into(), ctx.context.clone());

    return Ok(step);
}

fn iamb_replied(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    if !desc.arg.text.is_empty() {
        return Result::Err(CommandError::InvalidArgument);
    }

    let ract = IambAction::from(MessageAction::Replied);
    let step = CommandStep::Continue(ract.into(), ctx.context.clone());

    return Ok(step);
}

fn iamb_editor(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    if !desc.arg.text.is_empty() {
        return Result::Err(CommandError::InvalidArgument);
    }

    let sact = IambAction::from(SendAction::SubmitFromEditor);
    let step = CommandStep::Continue(sact.into(), ctx.context.clone());

    return Ok(step);
}

fn iamb_rooms(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    if !desc.arg.text.is_empty() {
        return Result::Err(CommandError::InvalidArgument);
    }

    let open = ctx.switch(OpenTarget::Application(IambId::RoomList));
    let step = CommandStep::Continue(open, ctx.context.clone());

    return Ok(step);
}

fn iamb_chats(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    if !desc.arg.text.is_empty() {
        return Result::Err(CommandError::InvalidArgument);
    }

    let open = ctx.switch(OpenTarget::Application(IambId::ChatList));
    let step = CommandStep::Continue(open, ctx.context.clone());

    return Ok(step);
}

fn iamb_read(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    let mut args = desc.arg.strings()?;

    if args.len() > 1 {
        return Result::Err(CommandError::InvalidArgument);
    }

    let act = match args.pop().as_deref() {
        // Mark the focused room read, or just the thread when viewing one.
        None => IambAction::Room(RoomAction::MarkRead),

        // Mark every room and thread read.
        Some("all") => IambAction::ClearUnreads,

        Some(_) => return Result::Err(CommandError::InvalidArgument),
    };

    let step = CommandStep::Continue(act.into(), ctx.context.clone());

    return Ok(step);
}

fn iamb_undoread(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    if !desc.arg.text.is_empty() {
        return Result::Err(CommandError::InvalidArgument);
    }

    let act = IambAction::UndoRead;
    let step = CommandStep::Continue(act.into(), ctx.context.clone());

    return Ok(step);
}

fn iamb_commands(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    if !desc.arg.text.is_empty() {
        return Result::Err(CommandError::InvalidArgument);
    }

    let open = ctx.switch(OpenTarget::Application(IambId::CommandPalette));
    let step = CommandStep::Continue(open, ctx.context.clone());

    return Ok(step);
}

fn iamb_switch(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    if !desc.arg.text.is_empty() {
        return Result::Err(CommandError::InvalidArgument);
    }

    let open = ctx.switch(OpenTarget::Application(IambId::QuickSwitcher));
    let step = CommandStep::Continue(open, ctx.context.clone());

    return Ok(step);
}

fn iamb_threads(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    if !desc.arg.text.is_empty() {
        return Result::Err(CommandError::InvalidArgument);
    }

    let open = ctx.switch(OpenTarget::Application(IambId::ThreadList));
    let step = CommandStep::Continue(open, ctx.context.clone());

    return Ok(step);
}

fn iamb_unreads_and_threads(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    if !desc.arg.text.is_empty() {
        return Result::Err(CommandError::InvalidArgument);
    }

    let open = ctx.switch(OpenTarget::Application(IambId::UnreadThreadList));
    let step = CommandStep::Continue(open, ctx.context.clone());

    return Ok(step);
}

fn iamb_unreads(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    let mut args = desc.arg.strings()?;

    if args.len() > 1 {
        return Result::Err(CommandError::InvalidArgument);
    }

    match args.pop().as_deref() {
        Some("clear") => {
            let clear = IambAction::ClearUnreads;
            let step = CommandStep::Continue(clear.into(), ctx.context.clone());

            return Ok(step);
        },
        Some("threads") => {
            let open = ctx.switch(OpenTarget::Application(IambId::UnreadThreadList));
            let step = CommandStep::Continue(open, ctx.context.clone());

            return Ok(step);
        },
        Some(_) => return Result::Err(CommandError::InvalidArgument),
        None => {
            let open = ctx.switch(OpenTarget::Application(IambId::UnreadList));
            let step = CommandStep::Continue(open, ctx.context.clone());

            return Ok(step);
        },
    }
}

fn iamb_spaces(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    if !desc.arg.text.is_empty() {
        return Result::Err(CommandError::InvalidArgument);
    }

    let open = ctx.switch(OpenTarget::Application(IambId::SpaceList));
    let step = CommandStep::Continue(open, ctx.context.clone());

    return Ok(step);
}

fn iamb_welcome(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    if !desc.arg.text.is_empty() {
        return Result::Err(CommandError::InvalidArgument);
    }

    let open = ctx.switch(OpenTarget::Application(IambId::Welcome));
    let step = CommandStep::Continue(open, ctx.context.clone());

    return Ok(step);
}

fn iamb_join(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    let mut args = desc.arg.filenames()?;

    if args.len() != 1 {
        return Result::Err(CommandError::InvalidArgument);
    }

    let open = ctx.switch(args.remove(0));
    let step = CommandStep::Continue(open, ctx.context.clone());

    return Ok(step);
}

fn iamb_create(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    let args = desc.arg.options()?;
    let mut flags = CreateRoomFlags::NONE;
    let mut alias = None;
    let mut ct = CreateRoomType::Room;

    for arg in args {
        match arg {
            OptionType::Flag(name, Some(arg)) => {
                match name.as_str() {
                    "alias" => {
                        if alias.is_some() {
                            let msg = "Multiple ++alias arguments are not allowed";
                            let err = CommandError::Error(msg.into());

                            return Err(err);
                        } else {
                            alias = Some(arg);
                        }
                    },
                    _ => return Err(CommandError::InvalidArgument),
                }
            },
            OptionType::Flag(name, None) => {
                match name.as_str() {
                    "public" => flags |= CreateRoomFlags::PUBLIC,
                    "space" => ct = CreateRoomType::Space,
                    "enc" | "encrypted" => flags |= CreateRoomFlags::ENCRYPTED,
                    _ => return Err(CommandError::InvalidArgument),
                }
            },
            OptionType::Positional(_) => {
                let msg = ":create doesn't take any positional arguments";
                let err = CommandError::Error(msg.into());

                return Err(err);
            },
        }
    }

    let hact = HomeserverAction::CreateRoom(alias, ct, flags);
    let iact = IambAction::from(hact);
    let step = CommandStep::Continue(iact.into(), ctx.context.clone());

    return Ok(step);
}

fn iamb_room(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    let mut args = desc.arg.strings()?;

    if args.len() < 2 {
        return Result::Err(CommandError::InvalidArgument);
    }

    let field = args.remove(0);
    let action = args.remove(0);

    if args.len() > 1 {
        return Result::Err(CommandError::InvalidArgument);
    }

    let act: IambAction = match (field.as_str(), action.as_str(), args.pop()) {
        // :room dm set
        ("dm", "set", None) => RoomAction::SetDirect(true).into(),
        ("dm", "set", Some(_)) => return Result::Err(CommandError::InvalidArgument),

        // :room dm set
        ("dm", "unset", None) => RoomAction::SetDirect(false).into(),
        ("dm", "unset", Some(_)) => return Result::Err(CommandError::InvalidArgument),

        // :room [kick|ban|unban] <user>
        ("kick", u, r) => {
            RoomAction::MemberUpdate(MemberUpdateAction::Kick, u.into(), r, desc.bang).into()
        },
        ("ban", u, r) => {
            RoomAction::MemberUpdate(MemberUpdateAction::Ban, u.into(), r, desc.bang).into()
        },
        ("unban", u, r) => {
            RoomAction::MemberUpdate(MemberUpdateAction::Unban, u.into(), r, desc.bang).into()
        },

        // :room history set <visibility>
        ("history", "set", Some(s)) => RoomAction::Set(RoomField::History, s).into(),
        ("history", "set", None) => return Result::Err(CommandError::InvalidArgument),

        // :room history unset
        ("history", "unset", None) => RoomAction::Unset(RoomField::History).into(),
        ("history", "unset", Some(_)) => return Result::Err(CommandError::InvalidArgument),

        // :room history show
        ("history", "show", None) => RoomAction::Show(RoomField::History).into(),
        ("history", "show", Some(_)) => return Result::Err(CommandError::InvalidArgument),

        // :room name set <room-name>
        ("name", "set", Some(s)) => RoomAction::Set(RoomField::Name, s).into(),
        ("name", "set", None) => return Result::Err(CommandError::InvalidArgument),

        // :room name unset
        ("name", "unset", None) => RoomAction::Unset(RoomField::Name).into(),
        ("name", "unset", Some(_)) => return Result::Err(CommandError::InvalidArgument),

        // :room topic set <topic>
        ("topic", "set", Some(s)) => RoomAction::Set(RoomField::Topic, s).into(),
        ("topic", "set", None) => return Result::Err(CommandError::InvalidArgument),

        // :room topic unset
        ("topic", "unset", None) => RoomAction::Unset(RoomField::Topic).into(),
        ("topic", "unset", Some(_)) => return Result::Err(CommandError::InvalidArgument),

        // :room topic show
        ("topic", "show", None) => RoomAction::Show(RoomField::Topic).into(),
        ("topic", "show", Some(_)) => return Result::Err(CommandError::InvalidArgument),

        // :room tag set <tag-name>
        ("tag", "set", Some(s)) => RoomAction::Set(RoomField::Tag(tag_name(s)?), "".into()).into(),
        ("tag", "set", None) => return Result::Err(CommandError::InvalidArgument),

        // :room tag unset <tag-name>
        ("tag", "unset", Some(s)) => RoomAction::Unset(RoomField::Tag(tag_name(s)?)).into(),
        ("tag", "unset", None) => return Result::Err(CommandError::InvalidArgument),

        // :room notify set <notification-level>
        ("notify", "set", Some(s)) => RoomAction::Set(RoomField::NotificationMode, s).into(),
        ("notify", "set", None) => return Result::Err(CommandError::InvalidArgument),

        // :room notify unset <notification-level>
        ("notify", "unset", None) => RoomAction::Unset(RoomField::NotificationMode).into(),
        ("notify", "unset", Some(_)) => return Result::Err(CommandError::InvalidArgument),

        // :room notify show
        ("notify", "show", None) => RoomAction::Show(RoomField::NotificationMode).into(),
        ("notify", "show", Some(_)) => return Result::Err(CommandError::InvalidArgument),

        // :room aliases show
        ("alias", "show", None) => RoomAction::Show(RoomField::Aliases).into(),
        ("alias", "show", Some(_)) => return Result::Err(CommandError::InvalidArgument),

        // :room aliases unset <alias>
        ("alias", "unset", Some(s)) => RoomAction::Unset(RoomField::Alias(s)).into(),
        ("alias", "unset", None) => return Result::Err(CommandError::InvalidArgument),

        // :room aliases set <alias>
        ("alias", "set", Some(s)) => RoomAction::Set(RoomField::Alias(s), "".into()).into(),
        ("alias", "set", None) => return Result::Err(CommandError::InvalidArgument),

        // :room canonicalalias show
        ("canonicalalias" | "canon", "show", None) => {
            RoomAction::Show(RoomField::CanonicalAlias).into()
        },
        ("canonicalalias" | "canon", "show", Some(_)) => {
            return Result::Err(CommandError::InvalidArgument)
        },

        // :room canonicalalias set
        ("canonicalalias" | "canon", "set", Some(s)) => {
            RoomAction::Set(RoomField::CanonicalAlias, s).into()
        },
        ("canonicalalias" | "canon", "set", None) => {
            return Result::Err(CommandError::InvalidArgument)
        },

        // :room canonicalalias unset
        ("canonicalalias" | "canon", "unset", None) => {
            RoomAction::Unset(RoomField::CanonicalAlias).into()
        },
        ("canonicalalias" | "canon", "unset", Some(_)) => {
            return Result::Err(CommandError::InvalidArgument)
        },

        // :room id show
        ("id", "show", None) => RoomAction::Show(RoomField::Id).into(),
        ("id", "show", Some(_)) => return Result::Err(CommandError::InvalidArgument),

        _ => return Result::Err(CommandError::InvalidArgument),
    };

    let step = CommandStep::Continue(act.into(), ctx.context.clone());

    return Ok(step);
}

fn iamb_space(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    let mut args = desc.arg.options()?;

    if args.len() < 2 {
        return Err(CommandError::InvalidArgument);
    }

    let OptionType::Positional(field) = args.remove(0) else {
        return Err(CommandError::InvalidArgument);
    };
    let OptionType::Positional(action) = args.remove(0) else {
        return Err(CommandError::InvalidArgument);
    };

    let act: IambAction = match (field.as_str(), action.as_str()) {
        // :space child remove
        ("child", "remove") => {
            if !(args.is_empty()) {
                return Err(CommandError::InvalidArgument);
            }
            SpaceAction::RemoveChild.into()
        },
        // :space child set <child>
        ("child", "set") => {
            let mut order = None;
            let mut suggested = false;
            let mut raw_child = None;

            for arg in args {
                match arg {
                    OptionType::Flag(name, Some(arg)) => {
                        match name.as_str() {
                            "order" => {
                                if order.is_some() {
                                    let msg = "Multiple ++order arguments are not allowed";
                                    let err = CommandError::Error(msg.into());

                                    return Err(err);
                                } else {
                                    order = Some(arg);
                                }
                            },
                            _ => return Err(CommandError::InvalidArgument),
                        }
                    },
                    OptionType::Flag(name, None) => {
                        match name.as_str() {
                            "suggested" => suggested = true,
                            _ => return Err(CommandError::InvalidArgument),
                        }
                    },
                    OptionType::Positional(arg) => {
                        if raw_child.is_some() {
                            let msg = "Multiple room arguments are not allowed";
                            let err = CommandError::Error(msg.into());

                            return Err(err);
                        }
                        raw_child = Some(arg);
                    },
                }
            }

            let child = if let Some(child) = raw_child {
                OwnedRoomId::from_str(&child)
                    .map_err(|_| CommandError::Error("Invalid room id specified".into()))?
            } else {
                let msg = "Must specify a room to add";
                return Err(CommandError::Error(msg.into()));
            };

            SpaceAction::SetChild(child, order, suggested).into()
        },
        _ => return Result::Err(CommandError::InvalidArgument),
    };

    let step = CommandStep::Continue(act.into(), ctx.context.clone());

    return Ok(step);
}

fn iamb_upload(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    let mut args = desc.arg.strings()?;

    // Without a path, we upload whatever image the system clipboard is holding.
    let sact = match args.len() {
        0 => SendAction::UploadClipboard,
        1 => SendAction::Upload(args.remove(0)),
        _ => return Result::Err(CommandError::InvalidArgument),
    };

    let iact = IambAction::from(sact);
    let step = CommandStep::Continue(iact.into(), ctx.context.clone());

    return Ok(step);
}

fn iamb_download(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    let mut args = desc.arg.strings()?;

    if args.len() > 1 {
        return Result::Err(CommandError::InvalidArgument);
    }

    let mut flags = DownloadFlags::NONE;
    if desc.bang {
        flags |= DownloadFlags::FORCE;
    };
    let mact = MessageAction::Download(args.pop(), flags);
    let iact = IambAction::from(mact);
    let step = CommandStep::Continue(iact.into(), ctx.context.clone());

    return Ok(step);
}

fn iamb_open(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    let mut args = desc.arg.strings()?;

    if args.len() > 1 {
        return Result::Err(CommandError::InvalidArgument);
    }

    let mut flags = DownloadFlags::OPEN;
    if desc.bang {
        flags |= DownloadFlags::FORCE;
    };
    let mact = MessageAction::Download(args.pop(), flags);
    let iact = IambAction::from(mact);
    let step = CommandStep::Continue(iact.into(), ctx.context.clone());

    return Ok(step);
}

fn iamb_logout(desc: CommandDescription, ctx: &mut ProgContext) -> ProgResult {
    let args = desc.arg.strings()?;

    if args.is_empty() {
        return Result::Err(CommandError::Error("Missing username".to_string()));
    }
    if args.len() != 1 {
        return Result::Err(CommandError::InvalidArgument);
    }

    let iact = IambAction::from(HomeserverAction::Logout(args[0].clone(), desc.bang));
    let step = CommandStep::Continue(iact.into(), ctx.context.clone());

    return Ok(step);
}

/// One usable form of a command: what you type after the name, and what it does.
pub struct CommandForm {
    /// What follows the command name, e.g. `"notify set <level>"`.
    ///
    /// Anything in `<>` or `[]` is a placeholder the user has to fill in; everything before the
    /// first placeholder is literal, and is what the palette can type for them.
    pub args: Option<&'static str>,

    /// What this form does.
    pub description: &'static str,

    /// The window this form opens, if opening a window is all it does.
    ///
    /// This is what the [quick switcher][crate::windows::switcher] lists alongside rooms, so a
    /// form that opens a window has to say so here or it will not be reachable from the switcher.
    /// Windows that need to be told which room they are about, like `:members`, are left out:
    /// there is no one window for the switcher to jump to.
    pub window: Option<IambId>,
}

/// A form of a command that takes something after the command name.
const fn form(args: &'static str, description: &'static str) -> CommandForm {
    CommandForm { args: Some(args), description, window: None }
}

/// A form of a command that is just the command name.
const fn bare(description: &'static str) -> CommandForm {
    CommandForm { args: None, description, window: None }
}

/// A form that is just the command name, and opens a window.
const fn opens(description: &'static str, window: IambId) -> CommandForm {
    CommandForm { args: None, description, window: Some(window) }
}

/// A form that takes something after the command name, and opens a window.
const fn form_opens(
    args: &'static str,
    description: &'static str,
    window: IambId,
) -> CommandForm {
    CommandForm { args: Some(args), description, window: Some(window) }
}

/// One of iamb's own commands: how it gets registered, and every form the palette should list.
pub struct IambCommandInfo {
    /// The name typed after `:`.
    pub name: &'static str,

    /// Other names that run the same command.
    pub aliases: &'static [&'static str],

    /// The handler that parses the command and produces actions.
    pub f: CommandFunc<IambInfo>,

    /// Every usable form of the command, for the palette.
    pub forms: &'static [CommandForm],
}

/// Every command that iamb itself defines.
///
/// This is both what [setup_commands] registers and what the
/// [command palette][crate::windows::palette] lists, so the palette cannot drift out of sync with
/// what is actually runnable. Subcommands live in `forms` rather than getting their own entry,
/// since they all go through one handler.
pub const IAMB_COMMANDS: &[IambCommandInfo] = &[
    IambCommandInfo {
        name: "cancel",
        aliases: &[],
        f: iamb_cancel,
        forms: &[bare("Cancel the drafted message, including any reply")],
    },
    IambCommandInfo {
        name: "chats",
        aliases: &[],
        f: iamb_chats,
        forms: &[opens("List joined rooms and direct messages together", IambId::ChatList)],
    },
    IambCommandInfo {
        name: "commands",
        aliases: &["palette"],
        f: iamb_commands,
        forms: &[opens("List iamb's commands and the keys bound to them", IambId::CommandPalette)],
    },
    IambCommandInfo {
        name: "create",
        aliases: &[],
        f: iamb_create,
        forms: &[
            bare("Create a new room"),
            form("++alias=<alias>", "Create a room with the given alias"),
            form("++public", "Create a room anyone can join"),
            form("++space", "Create a space instead of a room"),
            form("++encrypted", "Create a room with encryption enabled"),
        ],
    },
    IambCommandInfo {
        name: "dms",
        aliases: &[],
        f: iamb_dms,
        forms: &[opens("List your direct messages", IambId::DirectList)],
    },
    IambCommandInfo {
        name: "download",
        aliases: &[],
        f: iamb_download,
        forms: &[
            bare("Download the attachment on the selected message"),
            form("<path>", "Download the attachment to a specific path"),
        ],
    },
    IambCommandInfo {
        name: "edit",
        aliases: &[],
        f: iamb_edit,
        forms: &[bare("Edit the selected message")],
    },
    IambCommandInfo {
        name: "editor",
        aliases: &[],
        f: iamb_editor,
        forms: &[bare("Compose the message in your $EDITOR")],
    },
    IambCommandInfo {
        name: "forget",
        aliases: &[],
        f: iamb_forget,
        forms: &[bare("Remove all left rooms from the internal database")],
    },
    IambCommandInfo {
        name: "invite",
        aliases: &[],
        f: iamb_invite,
        forms: &[
            bare("Accept, reject, or send an invitation to the focused room"),
            form("accept", "Accept the invitation to the focused room"),
            form("reject", "Reject the invitation to the focused room"),
            form("send <user>", "Invite a user to the focused room"),
        ],
    },
    IambCommandInfo {
        name: "join",
        aliases: &[],
        f: iamb_join,
        forms: &[form("<room>", "Join a room, or open it if already joined")],
    },
    IambCommandInfo {
        name: "keys",
        aliases: &[],
        f: iamb_keys,
        forms: &[
            form("export <path> <passphrase>", "Export and encrypt your E2EE room keys"),
            form("import <path> <passphrase>", "Import and decrypt E2EE room keys"),
        ],
    },
    IambCommandInfo {
        name: "leave",
        aliases: &[],
        f: iamb_leave,
        forms: &[bare("Leave the focused room")],
    },
    IambCommandInfo {
        name: "logout",
        aliases: &[],
        f: iamb_logout,
        forms: &[form("<user id>", "Log out of the current profile")],
    },
    IambCommandInfo {
        name: "members",
        aliases: &[],
        f: iamb_members,
        forms: &[bare("List the members of the focused room")],
    },
    IambCommandInfo {
        name: "open",
        aliases: &[],
        f: iamb_open,
        forms: &[
            bare("Open the link, or download and open the attachment"),
            form("<path>", "Download the attachment to a path, then open it"),
        ],
    },
    IambCommandInfo {
        name: "react",
        aliases: &[],
        f: iamb_react,
        forms: &[form("<shortcode>", "React to the selected message with an emoji")],
    },
    IambCommandInfo {
        name: "read",
        aliases: &[],
        f: iamb_read,
        forms: &[
            bare("Mark the focused room, thread, or selected list entry as read"),
            form("all", "Mark every room and thread as read"),
        ],
    },
    IambCommandInfo {
        name: "pipe",
        aliases: &[],
        f: iamb_pipe,
        forms: &[form(
            "<command>",
            "Send the selected messages to a shell command on its standard input",
        )],
    },
    IambCommandInfo {
        name: "redact",
        aliases: &[],
        f: iamb_redact,
        forms: &[
            bare("Redact the selected message"),
            form("<reason>", "Redact the selected message with a reason"),
        ],
    },
    IambCommandInfo {
        name: "replied",
        aliases: &[],
        f: iamb_replied,
        forms: &[bare("Jump to the message the selected one replied to")],
    },
    IambCommandInfo {
        name: "reply",
        aliases: &[],
        f: iamb_reply,
        forms: &[bare("Reply to the selected message")],
    },
    IambCommandInfo {
        name: "room",
        aliases: &[],
        f: iamb_room,
        forms: &[
            form("name set <name>", "Set the name of the focused room"),
            form("name unset", "Unset the name of the focused room"),
            form("dm set", "Mark the focused room as a direct message"),
            form("dm unset", "Mark the focused room as a normal room"),
            form("notify set <level>", "Set the notification level: mute, mentions, keywords, all"),
            form("notify unset", "Clear the room's notification setting"),
            form("notify show", "Show the room's notification setting"),
            form("tag set <tag>", "Add a tag to the focused room"),
            form("tag unset <tag>", "Remove a tag from the focused room"),
            form("topic set <topic>", "Set the topic of the focused room"),
            form("topic unset", "Unset the topic of the focused room"),
            form("topic show", "Show the topic of the focused room"),
            form("alias set <alias>", "Point a new alternative alias at the room"),
            form("alias unset <alias>", "Delete an alternative alias from the room"),
            form("alias show", "Show the room's alternative aliases"),
            form("id show", "Show the Matrix identifier for the room"),
            form("canon set <alias>", "Make an alias the room's canonical one"),
            form("canon unset <alias>", "Delete the room's canonical alias"),
            form("canon show", "Show the room's canonical alias"),
            form("ban <user> <reason>", "Ban a user from the room"),
            form("unban <user> <reason>", "Unban a user from the room"),
            form("kick <user> <reason>", "Kick a user from the room"),
        ],
    },
    IambCommandInfo {
        name: "rooms",
        aliases: &[],
        f: iamb_rooms,
        forms: &[opens("List the rooms you have joined", IambId::RoomList)],
    },
    IambCommandInfo {
        name: "space",
        aliases: &[],
        f: iamb_space,
        forms: &[
            form("child set <room id>", "Add a room to the focused space"),
            form("child remove", "Remove the selected room from the focused space"),
        ],
    },
    IambCommandInfo {
        name: "spaces",
        aliases: &[],
        f: iamb_spaces,
        forms: &[opens("List the spaces you have joined", IambId::SpaceList)],
    },
    IambCommandInfo {
        name: "switch",
        aliases: &["switcher"],
        f: iamb_switch,
        forms: &[opens("Jump to a room, DM, space, or window", IambId::QuickSwitcher)],
    },
    IambCommandInfo {
        name: "threads",
        aliases: &[],
        f: iamb_threads,
        forms: &[opens("List the threads you follow across all rooms", IambId::ThreadList)],
    },
    IambCommandInfo {
        name: "unreact",
        aliases: &[],
        f: iamb_unreact,
        forms: &[
            bare("Remove all of your reactions from the selected message"),
            form("<shortcode>", "Remove one of your reactions from the selected message"),
        ],
    },
    IambCommandInfo {
        name: "undoread",
        aliases: &[],
        f: iamb_undoread,
        forms: &[bare("Undo the most recent read, restoring the previous read markers")],
    },
    IambCommandInfo {
        name: "unreads",
        aliases: &[],
        f: iamb_unreads,
        forms: &[
            opens("List unread rooms", IambId::UnreadList),
            form("clear", "Mark all rooms as read"),
            form_opens(
                "threads",
                "List unread rooms and unread followed threads together",
                IambId::UnreadThreadList,
            ),
        ],
    },
    IambCommandInfo {
        name: "unreadsandthreads",
        aliases: &[],
        f: iamb_unreads_and_threads,
        forms: &[opens(
            "List unread rooms and unread followed threads together",
            IambId::UnreadThreadList,
        )],
    },
    IambCommandInfo {
        name: "upload",
        aliases: &[],
        f: iamb_upload,
        forms: &[
            form("<path>", "Upload a file to the focused room"),
            form(
                "",
                "Upload the system clipboard's image, captioned with the message bar's text",
            ),
        ],
    },
    IambCommandInfo {
        name: "verify",
        aliases: &[],
        f: iamb_verify,
        forms: &[
            opens("List ongoing E2EE verifications", IambId::VerifyList),
            form("request <user id>", "Request a new verification with a user"),
            form("accept <key>", "Accept a verification request"),
            form("confirm <key>", "Confirm an in-progress verification"),
            form("cancel <key>", "Cancel an in-progress verification"),
            form("mismatch <key>", "Reject a verification because the emoji do not match"),
        ],
    },
    IambCommandInfo {
        name: "welcome",
        aliases: &[],
        f: iamb_welcome,
        forms: &[opens("Show the iamb welcome window", IambId::Welcome)],
    },
];

fn add_iamb_commands(cmds: &mut ProgramCommands) {
    for cmd in IAMB_COMMANDS {
        cmds.add_command(ProgramCommand {
            name: cmd.name.into(),
            aliases: cmd.aliases.iter().map(|a| (*a).into()).collect(),
            f: cmd.f,
        });
    }
}

/// Initialize the default command state.
pub fn setup_commands() -> ProgramCommands {
    let mut cmds = ProgramCommands::default();

    add_iamb_commands(&mut cmds);

    return cmds;
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::{room_id, user_id};
    use modalkit::actions::WindowAction;
    use modalkit::editing::context::EditContext;

    #[test]
    fn test_cmd_read() {
        let mut cmds = setup_commands();
        let ctx = EditContext::default();

        // This shadows modalkit's unimplemented Vim `:read`, which has no meaning in iamb.
        let res = cmds.input_cmd(":read", ctx.clone()).unwrap();
        let act = IambAction::Room(RoomAction::MarkRead);
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd(":read all", ctx.clone()).unwrap();
        let act = IambAction::ClearUnreads;
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        assert!(cmds.input_cmd(":read bogus", ctx).is_err());
    }

    #[test]
    fn test_cmd_undoread() {
        let mut cmds = setup_commands();
        let ctx = EditContext::default();

        let act = IambAction::UndoRead;
        let res = cmds.input_cmd(":undoread", ctx.clone()).unwrap();
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        assert!(cmds.input_cmd(":undoread all", ctx).is_err());
    }

    #[test]
    fn test_cmd_upload() {
        let mut cmds = setup_commands();
        let ctx = EditContext::default();

        let res = cmds.input_cmd(":upload /tmp/pic.png", ctx.clone()).unwrap();
        let act = IambAction::from(SendAction::Upload("/tmp/pic.png".into()));
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        // No path means the image sitting in the system clipboard.
        let res = cmds.input_cmd(":upload", ctx.clone()).unwrap();
        let act = IambAction::from(SendAction::UploadClipboard);
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        assert!(cmds.input_cmd(":upload /tmp/one.png /tmp/two.png", ctx).is_err());
    }

    #[test]
    fn test_cmd_commands() {
        let mut cmds = setup_commands();
        let ctx = EditContext::default();

        let act = WindowAction::Switch(OpenTarget::Application(IambId::CommandPalette));

        let res = cmds.input_cmd(":commands", ctx.clone()).unwrap();
        assert_eq!(res, vec![(act.clone().into(), ctx.clone())]);

        let res = cmds.input_cmd(":palette", ctx.clone()).unwrap();
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        assert!(cmds.input_cmd(":commands bogus", ctx).is_err());
    }

    #[test]
    fn test_cmd_threads() {
        let mut cmds = setup_commands();
        let ctx = EditContext::default();

        let res = cmds.input_cmd(":threads", ctx.clone()).unwrap();
        let act = WindowAction::Switch(OpenTarget::Application(IambId::ThreadList));
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        // Command names can't contain `-`, so the intermixed window is reachable both as
        // `:unreadsandthreads` and as a subcommand of the existing `:unreads`.
        let act = WindowAction::Switch(OpenTarget::Application(IambId::UnreadThreadList));

        let res = cmds.input_cmd(":unreadsandthreads", ctx.clone()).unwrap();
        assert_eq!(res, vec![(act.clone().into(), ctx.clone())]);

        let res = cmds.input_cmd(":unreads threads", ctx.clone()).unwrap();
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        assert!(cmds.input_cmd(":threads foo", ctx.clone()).is_err());
        assert!(cmds.input_cmd(":unreadsandthreads foo", ctx).is_err());
    }

    #[test]
    fn test_cmd_switch() {
        let mut cmds = setup_commands();
        let ctx = EditContext::default();
        let act = WindowAction::Switch(OpenTarget::Application(IambId::QuickSwitcher));

        let res = cmds.input_cmd(":switch", ctx.clone()).unwrap();
        assert_eq!(res, vec![(act.clone().into(), ctx.clone())]);

        let res = cmds.input_cmd(":switcher", ctx.clone()).unwrap();
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        assert!(cmds.input_cmd(":switch foo", ctx).is_err());
    }

    #[test]
    fn test_every_form_that_opens_a_window_is_bare_or_literal() {
        // The switcher shows these forms as-is and jumps straight to the window, so a form that
        // still needed the user to fill something in would be showing them a lie.
        for form in IAMB_COMMANDS.iter().flat_map(|cmd| cmd.forms) {
            if form.window.is_none() {
                continue;
            }

            let args = form.args.unwrap_or_default();

            assert!(
                !args.contains('<') && !args.contains('['),
                "{:?} opens a window but takes an argument",
                args
            );
        }
    }

    #[test]
    fn test_cmd_verify() {
        let mut cmds = setup_commands();
        let ctx = EditContext::default();

        let res = cmds.input_cmd(":verify", ctx.clone()).unwrap();
        let act = WindowAction::Switch(OpenTarget::Application(IambId::VerifyList));
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd(":verify request @user1:example.com", ctx.clone()).unwrap();
        let act = IambAction::VerifyRequest("@user1:example.com".into());
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds
            .input_cmd(":verify accept @user1:example.com/FOOBAR", ctx.clone())
            .unwrap();
        let act = IambAction::Verify(VerifyAction::Accept, "@user1:example.com/FOOBAR".into());
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds
            .input_cmd(":verify mismatch @user2:example.com/QUUXBAZ", ctx.clone())
            .unwrap();
        let act = IambAction::Verify(VerifyAction::Mismatch, "@user2:example.com/QUUXBAZ".into());
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds
            .input_cmd(":verify cancel @user3:example.com/MYDEVICE", ctx.clone())
            .unwrap();
        let act = IambAction::Verify(VerifyAction::Cancel, "@user3:example.com/MYDEVICE".into());
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds
            .input_cmd(":verify confirm @user4:example.com/GOODDEV", ctx.clone())
            .unwrap();
        let act = IambAction::Verify(VerifyAction::Confirm, "@user4:example.com/GOODDEV".into());
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd(":verify confirm", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));

        let res = cmds.input_cmd(":verify cancel @user4:example.com MYDEVICE", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));

        let res = cmds.input_cmd(":verify mismatch a b c d e f", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));
    }

    #[test]
    fn test_cmd_join() {
        let mut cmds = setup_commands();
        let ctx = EditContext::default();

        let res = cmds.input_cmd("join #foobar:example.com", ctx.clone()).unwrap();
        let act = WindowAction::Switch(OpenTarget::Name("#foobar:example.com".into()));
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("join #", ctx.clone()).unwrap();
        let act = WindowAction::Switch(OpenTarget::Alternate);
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("join", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));

        let res = cmds.input_cmd("join foo bar", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));
    }

    #[test]
    fn test_cmd_room_invalid() {
        let mut cmds = setup_commands();
        let ctx = EditContext::default();

        let res = cmds.input_cmd("room", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));

        let res = cmds.input_cmd("room foo", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));

        let res = cmds.input_cmd("room set topic", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));
    }

    #[test]
    fn test_cmd_room_topic_set() {
        let mut cmds = setup_commands();
        let ctx = EditContext::default();

        let res = cmds
            .input_cmd("room topic set \"Lots of fun discussion!\"", ctx.clone())
            .unwrap();
        let act = RoomAction::Set(RoomField::Topic, "Lots of fun discussion!".into());
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds
            .input_cmd("room topic set The\\ Discussion\\ Room", ctx.clone())
            .unwrap();
        let act = RoomAction::Set(RoomField::Topic, "The Discussion Room".into());
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room topic set Development", ctx.clone()).unwrap();
        let act = RoomAction::Set(RoomField::Topic, "Development".into());
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room topic", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));

        let res = cmds.input_cmd("room topic set", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));

        let res = cmds.input_cmd("room topic set A B C", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));
    }

    #[test]
    fn test_cmd_room_name_invalid() {
        let mut cmds = setup_commands();
        let ctx = EditContext::default();

        let res = cmds.input_cmd("room name", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));

        let res = cmds.input_cmd("room name foo", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));
    }

    #[test]
    fn test_cmd_room_name_set() {
        let mut cmds = setup_commands();
        let ctx = EditContext::default();

        let res = cmds.input_cmd("room name set Development", ctx.clone()).unwrap();
        let act = RoomAction::Set(RoomField::Name, "Development".into());
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds
            .input_cmd("room name set \"Application Development\"", ctx.clone())
            .unwrap();
        let act = RoomAction::Set(RoomField::Name, "Application Development".into());
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room name set", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));
    }

    #[test]
    fn test_cmd_room_name_unset() {
        let mut cmds = setup_commands();
        let ctx = EditContext::default();

        let res = cmds.input_cmd("room name unset", ctx.clone()).unwrap();
        let act = RoomAction::Unset(RoomField::Name);
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room name unset foo", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));
    }

    #[test]
    fn test_cmd_room_dm_set() {
        let mut cmds = setup_commands();
        let ctx = EditContext::default();

        let res = cmds.input_cmd("room dm set", ctx.clone()).unwrap();
        let act = RoomAction::SetDirect(true);
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room dm set true", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));
    }

    #[test]
    fn test_cmd_room_dm_unset() {
        let mut cmds = setup_commands();
        let ctx = EditContext::default();

        let res = cmds.input_cmd("room dm unset", ctx.clone()).unwrap();
        let act = RoomAction::SetDirect(false);
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room dm unset true", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));
    }

    #[test]
    fn test_cmd_room_tag_set() {
        let mut cmds = setup_commands();
        let ctx = EditContext::default();

        let res = cmds.input_cmd("room tag set favourite", ctx.clone()).unwrap();
        let act = RoomAction::Set(RoomField::Tag(TagName::Favorite), "".into());
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room tag set favorite", ctx.clone()).unwrap();
        let act = RoomAction::Set(RoomField::Tag(TagName::Favorite), "".into());
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room tag set fav", ctx.clone()).unwrap();
        let act = RoomAction::Set(RoomField::Tag(TagName::Favorite), "".into());
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room tag set low_priority", ctx.clone()).unwrap();
        let act = RoomAction::Set(RoomField::Tag(TagName::LowPriority), "".into());
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room tag set low-priority", ctx.clone()).unwrap();
        let act = RoomAction::Set(RoomField::Tag(TagName::LowPriority), "".into());
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room tag set low", ctx.clone()).unwrap();
        let act = RoomAction::Set(RoomField::Tag(TagName::LowPriority), "".into());
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room tag set servernotice", ctx.clone()).unwrap();
        let act = RoomAction::Set(RoomField::Tag(TagName::ServerNotice), "".into());
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room tag set server_notice", ctx.clone()).unwrap();
        let act = RoomAction::Set(RoomField::Tag(TagName::ServerNotice), "".into());
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room tag set server_notice", ctx.clone()).unwrap();
        let act = RoomAction::Set(RoomField::Tag(TagName::ServerNotice), "".into());
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room tag set u.custom-tag", ctx.clone()).unwrap();
        let act = RoomAction::Set(
            RoomField::Tag(TagName::User("u.custom-tag".parse().unwrap())),
            "".into(),
        );
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room tag set u.irc", ctx.clone()).unwrap();
        let act =
            RoomAction::Set(RoomField::Tag(TagName::User("u.irc".parse().unwrap())), "".into());
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room tag", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));

        let res = cmds.input_cmd("room tag set", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));

        let res = cmds.input_cmd("room tag set unknown", ctx.clone());
        assert_eq!(res, Err(CommandError::Error("Invalid user tag name: unknown".into())));

        let res = cmds.input_cmd("room tag set needs-leading-u-dot", ctx.clone());
        assert_eq!(
            res,
            Err(CommandError::Error("Invalid user tag name: needs-leading-u-dot".into()))
        );
    }

    #[test]
    fn test_cmd_room_tag_unset() {
        let mut cmds = setup_commands();
        let ctx = EditContext::default();

        let res = cmds.input_cmd("room tag unset favourite", ctx.clone()).unwrap();
        let act = RoomAction::Unset(RoomField::Tag(TagName::Favorite));
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room tag unset favorite", ctx.clone()).unwrap();
        let act = RoomAction::Unset(RoomField::Tag(TagName::Favorite));
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room tag unset fav", ctx.clone()).unwrap();
        let act = RoomAction::Unset(RoomField::Tag(TagName::Favorite));
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room tag unset low_priority", ctx.clone()).unwrap();
        let act = RoomAction::Unset(RoomField::Tag(TagName::LowPriority));
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room tag unset low-priority", ctx.clone()).unwrap();
        let act = RoomAction::Unset(RoomField::Tag(TagName::LowPriority));
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room tag unset low", ctx.clone()).unwrap();
        let act = RoomAction::Unset(RoomField::Tag(TagName::LowPriority));
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room tag unset servernotice", ctx.clone()).unwrap();
        let act = RoomAction::Unset(RoomField::Tag(TagName::ServerNotice));
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room tag unset server_notice", ctx.clone()).unwrap();
        let act = RoomAction::Unset(RoomField::Tag(TagName::ServerNotice));
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room tag unset server_notice", ctx.clone()).unwrap();
        let act = RoomAction::Unset(RoomField::Tag(TagName::ServerNotice));
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room tag unset u.custom-tag", ctx.clone()).unwrap();
        let act = RoomAction::Unset(RoomField::Tag(TagName::User("u.custom-tag".parse().unwrap())));
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room tag unset u.irc", ctx.clone()).unwrap();
        let act = RoomAction::Unset(RoomField::Tag(TagName::User("u.irc".parse().unwrap())));
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room tag", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));

        let res = cmds.input_cmd("room tag set", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));

        let res = cmds.input_cmd("room tag unset unknown", ctx.clone());
        assert_eq!(res, Err(CommandError::Error("Invalid user tag name: unknown".into())));

        let res = cmds.input_cmd("room tag unset needs-leading-u-dot", ctx.clone());
        assert_eq!(
            res,
            Err(CommandError::Error("Invalid user tag name: needs-leading-u-dot".into()))
        );
    }

    #[test]
    fn test_cmd_room_notification_mode_set() {
        let mut cmds = setup_commands();
        let ctx = EditContext::default();

        let cmd = "room notify set mute";
        let res = cmds.input_cmd(cmd, ctx.clone()).unwrap();
        let act = RoomAction::Set(RoomField::NotificationMode, "mute".into());
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let cmd = "room notify unset";
        let res = cmds.input_cmd(cmd, ctx.clone()).unwrap();
        let act = RoomAction::Unset(RoomField::NotificationMode);
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let cmd = "room notify show";
        let res = cmds.input_cmd(cmd, ctx.clone()).unwrap();
        let act = RoomAction::Show(RoomField::NotificationMode);
        assert_eq!(res, vec![(act.into(), ctx.clone())]);
    }

    #[test]
    fn test_cmd_room_id_show() {
        let mut cmds = setup_commands();
        let ctx = EditContext::default();

        let res = cmds.input_cmd("room id show", ctx.clone()).unwrap();
        let act = RoomAction::Show(RoomField::Id);
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room id show foo", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));
    }

    #[test]
    fn test_cmd_space_child() {
        let mut cmds = setup_commands();
        let ctx = EditContext::default();

        let cmd = "space";
        let res = cmds.input_cmd(cmd, ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));

        let cmd = "space ++foo bar baz";
        let res = cmds.input_cmd(cmd, ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));

        let cmd = "space child foo";
        let res = cmds.input_cmd(cmd, ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));
    }

    #[test]
    fn test_cmd_space_child_set() {
        let mut cmds = setup_commands();
        let ctx = EditContext::default();

        let cmd = "space child set !roomid:example.org";
        let res = cmds.input_cmd(cmd, ctx.clone()).unwrap();
        let act = SpaceAction::SetChild(room_id!("!roomid:example.org").to_owned(), None, false);
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let cmd = "space child set ++order=abcd ++suggested !roomid:example.org";
        let res = cmds.input_cmd(cmd, ctx.clone()).unwrap();
        let act = SpaceAction::SetChild(
            room_id!("!roomid:example.org").to_owned(),
            Some("abcd".into()),
            true,
        );
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let cmd = "space child set ++order=abcd ++order=1234 !roomid:example.org";
        let res = cmds.input_cmd(cmd, ctx.clone());
        assert_eq!(
            res,
            Err(CommandError::Error("Multiple ++order arguments are not allowed".into()))
        );

        let cmd = "space child set !roomid:example.org !otherroom:example.org";
        let res = cmds.input_cmd(cmd, ctx.clone());
        assert_eq!(res, Err(CommandError::Error("Multiple room arguments are not allowed".into())));

        let cmd = "space child set ++foo=abcd !roomid:example.org";
        let res = cmds.input_cmd(cmd, ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));

        let cmd = "space child set ++foo !roomid:example.org";
        let res = cmds.input_cmd(cmd, ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));

        let cmd = "space child ++order=abcd ++suggested set !roomid:example.org";
        let res = cmds.input_cmd(cmd, ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));

        let cmd = "space child set foo";
        let res = cmds.input_cmd(cmd, ctx.clone());
        assert_eq!(res, Err(CommandError::Error("Invalid room id specified".into())));

        let cmd = "space child set";
        let res = cmds.input_cmd(cmd, ctx.clone());
        assert_eq!(res, Err(CommandError::Error("Must specify a room to add".into())));
    }

    #[test]
    fn test_cmd_space_child_remove() {
        let mut cmds = setup_commands();
        let ctx = EditContext::default();

        let cmd = "space child remove";
        let res = cmds.input_cmd(cmd, ctx.clone()).unwrap();
        let act = SpaceAction::RemoveChild;
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let cmd = "space child remove foo";
        let res = cmds.input_cmd(cmd, ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));
    }

    #[test]
    fn test_cmd_invite() {
        let mut cmds = setup_commands();
        let ctx = EditContext::default();

        let res = cmds.input_cmd("invite accept", ctx.clone()).unwrap();
        let act = IambAction::Room(RoomAction::InviteAccept);
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("invite reject", ctx.clone()).unwrap();
        let act = IambAction::Room(RoomAction::InviteReject);
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("invite send @user:example.com", ctx.clone()).unwrap();
        let act =
            IambAction::Room(RoomAction::InviteSend(user_id!("@user:example.com").to_owned()));
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("invite", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));

        let res = cmds.input_cmd("invite foo", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));

        let res = cmds.input_cmd("invite accept @user:example.com", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));

        let res = cmds.input_cmd("invite reject @user:example.com", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));

        let res = cmds.input_cmd("invite send", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));

        let res = cmds.input_cmd("invite @user:example.com", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));
    }

    #[test]
    fn test_cmd_room_kick() {
        let mut cmds = setup_commands();
        let ctx = EditContext::default();

        let res = cmds.input_cmd("room kick @user:example.com", ctx.clone()).unwrap();
        let act = IambAction::Room(RoomAction::MemberUpdate(
            MemberUpdateAction::Kick,
            "@user:example.com".into(),
            None,
            false,
        ));
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("room! kick @user:example.com", ctx.clone()).unwrap();
        let act = IambAction::Room(RoomAction::MemberUpdate(
            MemberUpdateAction::Kick,
            "@user:example.com".into(),
            None,
            true,
        ));
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds
            .input_cmd("room! kick @user:example.com \"reason here\"", ctx.clone())
            .unwrap();
        let act = IambAction::Room(RoomAction::MemberUpdate(
            MemberUpdateAction::Kick,
            "@user:example.com".into(),
            Some("reason here".into()),
            true,
        ));
        assert_eq!(res, vec![(act.into(), ctx.clone())]);
    }

    #[test]
    fn test_cmd_room_ban_unban() {
        let mut cmds = setup_commands();
        let ctx = EditContext::default();

        let res = cmds
            .input_cmd("room! ban @user:example.com \"spam\"", ctx.clone())
            .unwrap();
        let act = IambAction::Room(RoomAction::MemberUpdate(
            MemberUpdateAction::Ban,
            "@user:example.com".into(),
            Some("spam".into()),
            true,
        ));
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds
            .input_cmd("room unban @user:example.com \"reconciled\"", ctx.clone())
            .unwrap();
        let act = IambAction::Room(RoomAction::MemberUpdate(
            MemberUpdateAction::Unban,
            "@user:example.com".into(),
            Some("reconciled".into()),
            false,
        ));
        assert_eq!(res, vec![(act.into(), ctx.clone())]);
    }

    #[test]
    fn test_cmd_redact() {
        let mut cmds = setup_commands();
        let ctx = EditContext::default();

        let res = cmds.input_cmd("redact", ctx.clone()).unwrap();
        let act = IambAction::Message(MessageAction::Redact(None, false));
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("redact!", ctx.clone()).unwrap();
        let act = IambAction::Message(MessageAction::Redact(None, true));
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("redact Removed", ctx.clone()).unwrap();
        let act = IambAction::Message(MessageAction::Redact(Some("Removed".into()), false));
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("redact \"Removed\"", ctx.clone()).unwrap();
        let act = IambAction::Message(MessageAction::Redact(Some("Removed".into()), false));
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("redact Removed Removed", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));
    }

    #[test]
    fn test_cmd_keys() {
        let mut cmds = setup_commands();
        let ctx = EditContext::default();

        let res = cmds.input_cmd("keys import /a/b/c pword", ctx.clone()).unwrap();
        let act = IambAction::Keys(KeysAction::Import("/a/b/c".into(), "pword".into()));
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        let res = cmds.input_cmd("keys export /a/b/c pword", ctx.clone()).unwrap();
        let act = IambAction::Keys(KeysAction::Export("/a/b/c".into(), "pword".into()));
        assert_eq!(res, vec![(act.into(), ctx.clone())]);

        // Invalid invocations.
        let res = cmds.input_cmd("keys", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));

        let res = cmds.input_cmd("keys import", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));

        let res = cmds.input_cmd("keys import foo", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));

        let res = cmds.input_cmd("keys import foo bar baz", ctx.clone());
        assert_eq!(res, Err(CommandError::InvalidArgument));
    }
}
