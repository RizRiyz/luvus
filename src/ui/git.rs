//! The git tab dashboard (docs/17, GIT-1): a header (repo · branch · ahead/
//! behind · dirty) + a view selector, the active section (Branches / Commits /
//! Status, with PRs/Issues arriving in GIT-2), and a footer of hints. Pure
//! ratatui, themed with the existing palette.

use super::*;
use crate::git::model::{Checks, IssueDetail, PrDetail, PullRequest};
use crate::git::{
    filtered_branches, filtered_commits, filtered_issues, filtered_prs, DetailTextCache,
    DetailTextRow, DetailTextTarget, GitView, Load, Scope, Section,
};
use crate::i18n::Catalog;

const PR_STATUS_W: usize = 13;
const PR_NUMBER_W: usize = 6;
const PR_AUTHOR_W: usize = 11;
const PR_REVIEWER_W: usize = 11;
const PR_CHECKS_W: usize = 8;
const PR_CHANGE_W: usize = 8;
const PR_FIXED_W: usize =
    PR_STATUS_W + PR_NUMBER_W + PR_AUTHOR_W + PR_REVIEWER_W + PR_CHECKS_W + PR_CHANGE_W * 2;

fn pr_title_width(row_width: usize) -> usize {
    row_width.saturating_sub(PR_FIXED_W).max(12)
}

/// The view-selector label for `s` in the active language.
fn section_label(s: Section, cat: &Catalog) -> &'static str {
    match s {
        Section::Commits => cat.sec_commits,
        Section::Flow => cat.sec_flow,
        Section::Branches => cat.sec_branches,
        Section::Prs => cat.sec_prs,
        Section::Issues => cat.sec_issues,
        Section::Status => cat.sec_status,
    }
}

/// The PR/issue scope label (`m` toggle) in the active language.
fn scope_label(s: Scope, cat: &Catalog) -> &'static str {
    match s {
        Scope::ThisRepo => cat.scope_this_repo,
        Scope::MyWork => cat.scope_my_work,
    }
}

/// Renders the git tab; returns the clickable view-selector rects so the input
/// layer can switch sections on a tab click.
pub(super) fn render(
    f: &mut RenderTarget,
    area: Rect,
    g: &mut GitView,
    compact: bool,
    cat: &Catalog,
    t: &Theme,
) -> Vec<(Section, Rect)> {
    if area.height < 4 || area.width < 12 {
        return Vec::new();
    }
    let tab_rects = draw_header(f, Rect::new(area.x, area.y, area.width, 1), g, cat, t);
    hline(f, area.x, area.y + 1, area.width, t);

    // On a phone (docs/18) the keyboard-hint footer and its separator are dropped
    // and their two rows go to the list; on desktop it renders exactly as before.
    let footer_h: u16 = if compact { 0 } else { 2 };
    if !compact {
        let footer_y = area.bottom().saturating_sub(1);
        hline(f, area.x, footer_y.saturating_sub(1), area.width, t);
        draw_footer(f, Rect::new(area.x, footer_y, area.width, 1), g, cat, t);
    }

    let body = Rect::new(
        area.x + 1,
        area.y + 2,
        area.width.saturating_sub(2),
        area.height.saturating_sub(2 + footer_h),
    );
    // Record the list body so a mouse click can map to a commit row (docs/17).
    g.list_area = body;
    // Only the Status view re-sets this; clear it so a rect from a previous frame
    // can't stay clickable after switching section or opening a detail view.
    g.contributors_more_rect = None;
    // The PR detail panel (GIT-6) overlays the section body when open; it scrolls
    // as a block like Flow/Status.
    if g.open_pr.is_some() {
        g.scroll = draw_pr_detail(f, body, g, cat, t);
        return tab_rects;
    }
    // The commit detail view (docs/17): `git show` in-tab, scrolls as a block.
    if g.open_commit.is_some() {
        g.scroll = draw_commit_detail(f, body, g, t);
        return tab_rects;
    }
    // The issue detail view (docs/17): in-tab, scrolls as a block.
    if g.open_issue.is_some() {
        g.scroll = draw_issue_detail(f, body, g, cat, t);
        return tab_rects;
    }
    // Flow / Status scroll as a block: they return the clamped scroll offset,
    // which we write back so the wheel/keys settle at the content's end.
    match g.section {
        Section::Commits => draw_commits(f, body, g, cat, t),
        Section::Flow => g.scroll = draw_flow(f, body, g, cat, t),
        Section::Prs => draw_prs(f, body, g, cat, t),
        Section::Issues => draw_issues(f, body, g, cat, t),
        Section::Branches => draw_branches(f, body, g, cat, t),
        Section::Status => {
            let (scroll, more) = draw_status(f, body, g, cat, t);
            g.scroll = scroll;
            g.contributors_more_rect = more;
        }
    }
    tab_rects
}

fn draw_prs(f: &mut RenderTarget, area: Rect, g: &GitView, cat: &Catalog, t: &Theme) {
    let v = match &g.prs {
        Load::Idle => {
            let note = g.gh.note().unwrap_or(cat.git_unavailable);
            return message(
                f,
                area,
                &format!("{} — {note}", cat.git_github_prs),
                t.overlay0,
            );
        }
        Load::Loading => return message(f, area, cat.git_loading_prs, t.overlay0),
        Load::Error(e) => return message(f, area, &format!("gh: {e}"), t.coral),
        Load::Loaded(v) => v,
    };
    if v.is_empty() {
        return message(f, area, cat.git_no_prs, t.green);
    }
    // `draw_list` reserves two cells for the selection marker. Calculate and
    // render the header in that same content width, so its columns align exactly
    // with the selectable rows below.
    let row_width = area.width.saturating_sub(2) as usize;
    let title_w = pr_title_width(row_width);
    let header = Line::from(vec![
        Span::styled(
            pad(cat.col_status, PR_STATUS_W),
            Style::new().fg(t.subtext0),
        ),
        Span::styled(pad("#", PR_NUMBER_W), Style::new().fg(t.subtext0)),
        Span::styled(pad(cat.col_title, title_w), Style::new().fg(t.subtext0)),
        Span::styled(
            pad(cat.col_author, PR_AUTHOR_W),
            Style::new().fg(t.subtext0),
        ),
        Span::styled(
            pad(cat.col_reviewer, PR_REVIEWER_W),
            Style::new().fg(t.subtext0),
        ),
        Span::styled(
            pad(cat.col_checks, PR_CHECKS_W),
            Style::new().fg(t.subtext0),
        ),
        Span::styled(pad("+", PR_CHANGE_W), Style::new().fg(t.subtext0)),
        Span::styled(pad("-", PR_CHANGE_W), Style::new().fg(t.subtext0)),
    ]);
    f.render_widget(
        Paragraph::new(header),
        Rect::new(area.x + 2, area.y, area.width.saturating_sub(2), 1),
    );
    let list = Rect::new(
        area.x,
        area.y + 1,
        area.width,
        area.height.saturating_sub(1),
    );
    let rows: Vec<Line> = filtered_prs(v, &g.filter)
        .map(|p| pr_line(p, title_w, cat, t))
        .collect();
    draw_list(f, list, rows, g.cursor, cat, t);
}

fn pr_line(p: &PullRequest, title_w: usize, cat: &Catalog, t: &Theme) -> Line<'static> {
    let (badge, bcol) = pr_badge(p, cat, t);
    let (gly, ccol) = check_glyph(p.checks, t);
    let reviewer = p.reviewers.first().map(String::as_str).unwrap_or("");
    // In "my work" scope each PR carries its repo; show it before the title.
    let title = if p.repo.is_empty() {
        p.title.clone()
    } else {
        format!("{}  {}", p.repo, p.title)
    };
    Line::from(vec![
        Span::styled(
            pad(&format!("[{badge}]"), PR_STATUS_W),
            Style::new().fg(bcol).bold(),
        ),
        Span::styled(
            pad(&format!("#{}", p.number), PR_NUMBER_W),
            Style::new().fg(t.subtext0),
        ),
        Span::styled(pad(&title, title_w), Style::new().fg(t.text)),
        Span::styled(pad(&p.author, PR_AUTHOR_W), Style::new().fg(t.subtext0)),
        Span::styled(pad(reviewer, PR_REVIEWER_W), Style::new().fg(t.amber)),
        Span::styled(pad(gly, PR_CHECKS_W), Style::new().fg(ccol)),
        Span::styled(
            pad(&format!("+{}", p.additions), PR_CHANGE_W),
            Style::new().fg(t.green),
        ),
        Span::styled(
            pad(&format!("-{}", p.deletions), PR_CHANGE_W),
            Style::new().fg(t.coral),
        ),
    ])
}

/// PR status badge text + color (from draft/state/reviewDecision).
fn pr_badge(p: &PullRequest, cat: &Catalog, t: &Theme) -> (&'static str, Color) {
    if p.is_draft {
        (cat.badge_draft, t.overlay0)
    } else if p.state == "MERGED" {
        (cat.badge_merged, t.accent)
    } else {
        match p.review_decision.as_str() {
            "APPROVED" => (cat.badge_approved, t.green),
            "CHANGES_REQUESTED" => (cat.badge_denied, t.coral),
            "REVIEW_REQUIRED" => (cat.badge_review, t.amber),
            _ => (cat.badge_open, t.subtext0),
        }
    }
}

fn check_glyph(c: Checks, t: &Theme) -> (&'static str, Color) {
    match c {
        Checks::Passing => ("✓", t.green),
        Checks::Failing => ("✗", t.coral),
        Checks::Pending => ("●", t.amber),
        Checks::None => ("—", t.overlay0),
    }
}

const DETAIL_SPLIT_MIN_WIDTH: u16 = 90;

fn detail_columns(area: Rect) -> Option<(Rect, u16, Rect)> {
    if area.width < DETAIL_SPLIT_MIN_WIDTH {
        return None;
    }
    let rail_width = area.width / 3;
    let main_width = area.width.saturating_sub(rail_width + 2);
    if main_width < 48 || rail_width < 26 {
        return None;
    }
    let main = Rect::new(area.x, area.y, main_width, area.height);
    let divider = main.right();
    let rail = Rect::new(divider.saturating_add(2), area.y, rail_width, area.height);
    Some((main, divider, rail))
}

fn detail_heading(title: impl Into<String>, t: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        title.into(),
        Style::new().fg(t.subtext1).bold(),
    ))
}

fn push_detail_title(
    rows: &mut Vec<Line<'static>>,
    number: u64,
    title: &str,
    width: usize,
    t: &Theme,
) {
    let prefix = format!("#{number}  ");
    let indent = " ".repeat(prefix.chars().count());
    for (index, line) in wrap(title, width.saturating_sub(prefix.chars().count()))
        .into_iter()
        .enumerate()
    {
        rows.push(if index == 0 {
            Line::from(vec![
                Span::styled(prefix.clone(), Style::new().fg(t.subtext0)),
                Span::styled(line, Style::new().fg(t.text).bold()),
            ])
        } else {
            Line::from(vec![
                Span::raw(indent.clone()),
                Span::styled(line, Style::new().fg(t.text).bold()),
            ])
        });
    }
}

fn update_detail_text_cache(
    cache: &mut Option<DetailTextCache>,
    target: DetailTextTarget,
    width: usize,
    body: &str,
    comments: &[crate::git::model::DiscussionComment],
    total: u64,
) -> bool {
    if cache
        .as_ref()
        .is_some_and(|cached| cached.target == target && cached.width == width)
    {
        return false;
    }
    *cache = Some(DetailTextCache {
        target,
        width,
        rows: DetailTextCache::rows(width, body, comments, total, wrap),
    });
    true
}

fn detail_text_line<'a>(row: &'a DetailTextRow, cat: &'a Catalog, t: &Theme) -> Line<'a> {
    match row {
        DetailTextRow::DescriptionHeading => detail_heading(cat.detail_description, t),
        DetailTextRow::EmptyDescription => Line::from(vec![
            Span::raw("   "),
            Span::styled(cat.detail_no_description, Style::new().fg(t.overlay0)),
        ]),
        DetailTextRow::Description(line) => {
            Line::from(Span::styled(line.as_str(), Style::new().fg(t.subtext0)))
        }
        DetailTextRow::Blank => Line::from(""),
        DetailTextRow::CommentsHeading { shown, total } => {
            let count = if shown < total {
                format!("{shown}/{total}")
            } else {
                total.to_string()
            };
            detail_heading(format!("{} ({count})", title_case(cat.detail_comments)), t)
        }
        DetailTextRow::CommentHeader { author, date } => Line::from(vec![
            Span::styled(author.as_str(), Style::new().fg(t.text).bold()),
            Span::styled(
                if date.is_empty() {
                    String::new()
                } else {
                    format!("  ·  {date}")
                },
                Style::new().fg(t.overlay1),
            ),
        ]),
        DetailTextRow::EmptyComment => Line::from(vec![
            Span::styled("│ ", Style::new().fg(t.accent)),
            Span::styled(cat.detail_no_description, Style::new().fg(t.overlay0)),
        ]),
        DetailTextRow::CommentBody(line) => Line::from(vec![
            Span::styled("│ ", Style::new().fg(t.accent)),
            Span::styled(line.as_str(), Style::new().fg(t.subtext0)),
        ]),
    }
}

fn title_case(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

struct DetailRows<'a> {
    prefix: &'a [Line<'static>],
    discussion: &'a [DetailTextRow],
    peer_row_count: usize,
}

fn render_scrolled_detail(
    f: &mut RenderTarget,
    area: Rect,
    rows: DetailRows<'_>,
    requested_scroll: usize,
    cat: &Catalog,
    t: &Theme,
) -> usize {
    let available = area.height as usize;
    let row_count = rows.prefix.len() + rows.discussion.len();
    let scroll = requested_scroll.min(row_count.max(rows.peer_row_count).saturating_sub(available));
    for (y, index) in (area.y..).zip((scroll..row_count).take(available)) {
        let row = Rect::new(area.x, y, area.width, 1);
        if let Some(line) = rows.prefix.get(index) {
            f.render_widget(Paragraph::new(line.clone()), row);
        } else {
            let line = detail_text_line(&rows.discussion[index - rows.prefix.len()], cat, t);
            f.render_widget(Paragraph::new(line), row);
        }
    }
    scroll
}

fn render_detail_rail(f: &mut RenderTarget, area: Rect, rows: Vec<Line<'static>>, scroll: usize) {
    let available = area.height as usize;
    let scroll = scroll.min(rows.len().saturating_sub(available));
    for (y, line) in (area.y..).zip(rows.into_iter().skip(scroll).take(available)) {
        f.render_widget(Paragraph::new(line), Rect::new(area.x, y, area.width, 1));
    }
}

fn draw_detail_divider(f: &mut RenderTarget, x: u16, area: Rect, t: &Theme) {
    let buffer = f.buffer_mut();
    for y in area.y..area.bottom() {
        buffer[(x, y)]
            .set_symbol("│")
            .set_style(Style::new().fg(t.surface1).bg(t.mantle));
    }
}

/// The PR detail panel keeps the discussion in a two-thirds reading column and
/// stable metadata in a one-third rail. Narrow screens stack the same content.
fn draw_pr_detail(
    f: &mut RenderTarget,
    area: Rect,
    g: &mut GitView,
    cat: &Catalog,
    t: &Theme,
) -> usize {
    let d = match &g.detail {
        Load::Loading => {
            message(f, area, cat.git_loading_pr, t.overlay0);
            return 0;
        }
        Load::Error(e) => {
            message(f, area, &format!("gh: {e}"), t.coral);
            return 0;
        }
        Load::Loaded(d) => d,
        Load::Idle => return 0,
    };
    let columns = detail_columns(area);
    let content_width = columns.map_or(area.width, |(main, _, _)| main.width) as usize;
    update_detail_text_cache(
        &mut g.detail_text_cache,
        DetailTextTarget::Pr(d.number),
        content_width,
        &d.body,
        &d.comments,
        d.comment_count,
    );
    let mut lead: Vec<Line> = Vec::new();
    let mut rail: Vec<Line> = Vec::new();

    let (badge, bcol) = detail_badge(d, cat, t);
    push_detail_title(&mut lead, d.number, &d.title, content_width, t);
    let updated = d.updated_at.split('T').next().unwrap_or("");
    let mut byline = vec![Span::styled(
        format!("{} {}", cat.detail_by, d.author),
        Style::new().fg(t.subtext0),
    )];
    if !updated.is_empty() {
        byline.push(Span::styled(
            format!("  · {} {updated}", cat.detail_updated),
            Style::new().fg(t.subtext0),
        ));
    }
    lead.push(Line::from(byline));
    lead.push(Line::from(""));

    rail.push(Line::from(Span::styled(
        format!("[{badge}]"),
        Style::new().fg(bcol).bold(),
    )));
    rail.push(Line::from(""));
    rail.push(detail_heading(title_case(cat.branch), t));
    rail.push(Line::from(Span::styled(
        d.head.clone(),
        Style::new().fg(t.accent).bold(),
    )));
    rail.push(Line::from(Span::styled(
        format!("→ {}", d.base),
        Style::new().fg(t.subtext0),
    )));
    if !d.labels.is_empty() {
        rail.push(Line::from(""));
        rail.push(detail_heading(cat.detail_labels, t));
        for label in &d.labels {
            rail.push(Line::from(Span::styled(
                format!("● {label}"),
                Style::new().fg(t.amber),
            )));
        }
    }
    rail.push(Line::from(""));
    rail.push(Line::from(vec![
        Span::styled(format!("+{} ", d.additions), Style::new().fg(t.green)),
        Span::styled(format!("-{}", d.deletions), Style::new().fg(t.coral)),
    ]));
    rail.push(Line::from(Span::styled(
        format!("{} {}", d.changed_files, cat.detail_files),
        Style::new().fg(t.subtext0),
    )));
    rail.push(Line::from(Span::styled(
        format!("{} {}", d.commits, cat.detail_commits),
        Style::new().fg(t.subtext0),
    )));
    rail.push(Line::from(Span::styled(
        format!("{} {}", d.comment_count, cat.detail_comments),
        Style::new().fg(t.subtext0),
    )));
    match d.mergeable.as_str() {
        "MERGEABLE" => rail.push(Line::from(Span::styled(
            format!("✓ {}", cat.detail_mergeable),
            Style::new().fg(t.green),
        ))),
        "CONFLICTING" => rail.push(Line::from(Span::styled(
            format!("✗ {}", cat.detail_conflicts),
            Style::new().fg(t.coral),
        ))),
        _ => {}
    }

    if !d.check_runs.is_empty() {
        rail.push(Line::from(""));
        rail.push(detail_heading(cat.detail_checks, t));
        for c in &d.check_runs {
            let (gly, col) = check_glyph(c.bucket, t);
            rail.push(Line::from(vec![
                Span::styled(format!("{gly}  "), Style::new().fg(col)),
                Span::styled(c.name.clone(), Style::new().fg(t.text)),
            ]));
        }
    }

    if !d.reviews.is_empty() {
        rail.push(Line::from(""));
        rail.push(detail_heading(cat.detail_reviews, t));
        for r in &d.reviews {
            let (gly, col, label) = review_glyph(&r.state, cat, t);
            rail.push(Line::from(vec![
                Span::styled(format!("{gly}  "), Style::new().fg(col)),
                Span::styled(r.author.clone(), Style::new().fg(t.text)),
                Span::styled(format!("  {label}"), Style::new().fg(col)),
            ]));
        }
    }

    if let Some((main, divider, sidebar)) = columns {
        let discussion = &g.detail_text_cache.as_ref().unwrap().rows;
        let scroll = render_scrolled_detail(
            f,
            main,
            DetailRows {
                prefix: &lead,
                discussion,
                peer_row_count: rail.len(),
            },
            g.scroll,
            cat,
            t,
        );
        draw_detail_divider(f, divider, area, t);
        render_detail_rail(f, sidebar, rail, scroll);
        scroll
    } else {
        lead.extend(rail);
        lead.push(Line::from(""));
        let discussion = &g.detail_text_cache.as_ref().unwrap().rows;
        render_scrolled_detail(
            f,
            area,
            DetailRows {
                prefix: &lead,
                discussion,
                peer_row_count: 0,
            },
            g.scroll,
            cat,
            t,
        )
    }
}

/// The in-tab commit detail (docs/17): the `git show` output, per-line colored
/// like a diff, scrolling as a block. Returns the clamped scroll offset.
fn draw_commit_detail(f: &mut RenderTarget, area: Rect, g: &GitView, t: &Theme) -> usize {
    let d = match &g.commit_detail {
        Load::Loading => {
            message(f, area, "loading…", t.overlay0);
            return 0;
        }
        Load::Error(e) => {
            message(f, area, &format!("git: {e}"), t.coral);
            return 0;
        }
        Load::Loaded(d) => d,
        Load::Idle => return 0,
    };
    let style = |line: &str| -> Style {
        if line.starts_with("commit ") {
            Style::new().fg(t.amber).bold()
        } else if line.starts_with("diff --git") || line.starts_with("index ") {
            Style::new().fg(t.subtext1).bold()
        } else if line.starts_with("@@") {
            Style::new().fg(t.mint)
        } else if line.starts_with("+++") || line.starts_with("---") {
            Style::new().fg(t.subtext0)
        } else if line.starts_with('+') {
            Style::new().fg(t.green)
        } else if line.starts_with('-') {
            Style::new().fg(t.coral)
        } else if line.starts_with("Author:")
            || line.starts_with("Date:")
            || line.starts_with("Merge:")
            || line.starts_with("Author")
        {
            Style::new().fg(t.subtext0)
        } else {
            Style::new().fg(t.text)
        }
    };
    let avail = area.height as usize;
    let scroll = g.scroll.min(d.lines.len().saturating_sub(avail));
    for (y, line) in (area.y..).zip(d.lines.iter().skip(scroll).take(avail)) {
        f.render_widget(
            Paragraph::new(Span::styled(line.clone(), style(line))),
            Rect::new(area.x, y, area.width, 1),
        );
    }
    scroll
}

/// The issue detail mirrors the PR reading layout: discussion on the left,
/// stable status and ownership metadata on the right, stacked when narrow.
fn draw_issue_detail(
    f: &mut RenderTarget,
    area: Rect,
    g: &mut GitView,
    cat: &Catalog,
    t: &Theme,
) -> usize {
    let d: &IssueDetail = match &g.issue_detail {
        Load::Loading => {
            message(f, area, "loading…", t.overlay0);
            return 0;
        }
        Load::Error(e) => {
            message(f, area, &format!("gh: {e}"), t.coral);
            return 0;
        }
        Load::Loaded(d) => d,
        Load::Idle => return 0,
    };
    let columns = detail_columns(area);
    let content_width = columns.map_or(area.width, |(main, _, _)| main.width) as usize;
    update_detail_text_cache(
        &mut g.detail_text_cache,
        DetailTextTarget::Issue(d.number),
        content_width,
        &d.body,
        &d.comments,
        d.comment_count,
    );
    let mut lead: Vec<Line> = Vec::new();
    let mut rail: Vec<Line> = Vec::new();

    push_detail_title(&mut lead, d.number, &d.title, content_width, t);
    let (badge, bcol) = if d.state.eq_ignore_ascii_case("CLOSED") {
        (cat.badge_closed, t.coral)
    } else {
        (cat.badge_open, t.green)
    };
    let updated = d.updated_at.split('T').next().unwrap_or("");
    let mut byline = vec![Span::styled(
        format!("{} {}", cat.detail_by, d.author),
        Style::new().fg(t.subtext0),
    )];
    if !updated.is_empty() {
        byline.push(Span::styled(
            format!("  · {} {updated}", cat.detail_updated),
            Style::new().fg(t.subtext0),
        ));
    }
    lead.push(Line::from(byline));
    lead.push(Line::from(""));

    rail.push(Line::from(Span::styled(
        format!("[{badge}]"),
        Style::new().fg(bcol).bold(),
    )));
    rail.push(Line::from(""));
    rail.push(Line::from(Span::styled(
        format!("{} {}", d.comment_count, cat.detail_comments),
        Style::new().fg(t.subtext0),
    )));
    if !d.labels.is_empty() {
        rail.push(Line::from(""));
        rail.push(detail_heading(cat.detail_labels, t));
        for label in &d.labels {
            rail.push(Line::from(Span::styled(
                format!("● {label}"),
                Style::new().fg(t.amber),
            )));
        }
    }
    if !d.assignees.is_empty() {
        rail.push(Line::from(""));
        rail.push(detail_heading(cat.detail_assignees, t));
        for assignee in &d.assignees {
            rail.push(Line::from(Span::styled(
                format!("@{assignee}"),
                Style::new().fg(t.subtext0),
            )));
        }
    }

    if let Some((main, divider, sidebar)) = columns {
        let discussion = &g.detail_text_cache.as_ref().unwrap().rows;
        let scroll = render_scrolled_detail(
            f,
            main,
            DetailRows {
                prefix: &lead,
                discussion,
                peer_row_count: rail.len(),
            },
            g.scroll,
            cat,
            t,
        );
        draw_detail_divider(f, divider, area, t);
        render_detail_rail(f, sidebar, rail, scroll);
        scroll
    } else {
        lead.extend(rail);
        lead.push(Line::from(""));
        let discussion = &g.detail_text_cache.as_ref().unwrap().rows;
        render_scrolled_detail(
            f,
            area,
            DetailRows {
                prefix: &lead,
                discussion,
                peer_row_count: 0,
            },
            g.scroll,
            cat,
            t,
        )
    }
}

/// Big state badge for the detail header.
fn detail_badge(d: &PrDetail, cat: &Catalog, t: &Theme) -> (&'static str, Color) {
    if d.state == "MERGED" {
        (cat.badge_merged, t.accent)
    } else if d.state == "CLOSED" {
        (cat.badge_closed, t.coral)
    } else if d.is_draft {
        (cat.badge_draft, t.overlay0)
    } else {
        match d.review_decision.as_str() {
            "APPROVED" => (cat.badge_approved, t.green),
            "CHANGES_REQUESTED" => (cat.badge_changes_requested, t.coral),
            "REVIEW_REQUIRED" => (cat.badge_review_required, t.amber),
            _ => (cat.badge_open, t.subtext0),
        }
    }
}

fn review_glyph(state: &str, cat: &Catalog, t: &Theme) -> (&'static str, Color, &'static str) {
    match state {
        "APPROVED" => ("✓", t.green, cat.rev_approved),
        "CHANGES_REQUESTED" => ("✗", t.coral, cat.rev_changes_requested),
        "COMMENTED" => ("○", t.subtext0, cat.rev_commented),
        "DISMISSED" => ("—", t.overlay0, cat.rev_dismissed),
        _ => ("·", t.subtext0, ""),
    }
}

/// Greedy word-wrap to `width` columns (whole words; over-long words pass
/// through and get clipped by the terminal). A blank input yields one blank line
/// so paragraph breaks survive.
fn wrap(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut line = String::new();
    for word in s.split_whitespace() {
        let word_width = word.chars().count();
        let mut chunks = word.chars();
        let mut first_chunk = true;
        loop {
            let chunk: String = chunks.by_ref().take(width).collect();
            if chunk.is_empty() {
                break;
            }
            if !first_chunk || word_width > width {
                if !line.is_empty() {
                    out.push(std::mem::take(&mut line));
                }
                if chunk.chars().count() == width {
                    out.push(chunk);
                } else {
                    line = chunk;
                }
            } else if line.is_empty() {
                line = chunk;
            } else if line.chars().count() + 1 + chunk.chars().count() <= width {
                line.push(' ');
                line.push_str(&chunk);
            } else {
                out.push(std::mem::take(&mut line));
                line = chunk;
            }
            first_chunk = false;
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn draw_issues(f: &mut RenderTarget, area: Rect, g: &GitView, cat: &Catalog, t: &Theme) {
    let v = match &g.issues {
        Load::Idle => {
            let note = g.gh.note().unwrap_or(cat.git_unavailable);
            return message(
                f,
                area,
                &format!("{} — {note}", cat.git_github_issues),
                t.overlay0,
            );
        }
        Load::Loading => return message(f, area, cat.git_loading_issues, t.overlay0),
        Load::Error(e) => return message(f, area, &format!("gh: {e}"), t.coral),
        Load::Loaded(v) => v,
    };
    if v.is_empty() {
        return message(f, area, cat.git_no_issues, t.green);
    }
    let title_w = area.width.saturating_sub(52).max(12) as usize;
    let rows: Vec<Line> = filtered_issues(v, &g.filter)
        .map(|i| {
            let assignee = i.assignees.first().map(String::as_str).unwrap_or("—");
            let title = if i.repo.is_empty() {
                i.title.clone()
            } else {
                format!("{}  {}", i.repo, i.title)
            };
            Line::from(vec![
                Span::styled(format!("#{:<5}", i.number), Style::new().fg(t.subtext0)),
                Span::styled(pad(&title, title_w), Style::new().fg(t.text)),
                Span::styled(pad(&i.author, 11), Style::new().fg(t.subtext0)),
                Span::styled(pad(assignee, 11), Style::new().fg(t.amber)),
                Span::styled(trunc(&i.labels.join(", "), 20), Style::new().fg(t.mint)),
            ])
        })
        .collect();
    draw_list(f, area, rows, g.cursor, cat, t);
}

/// The **flow** view: the trunk branch as a track, with the other branches
/// diverging below — each with its commit dots, ahead/behind, and matched PR.
/// A GitHub-flow-style picture built from the data already fetched.
fn draw_flow(f: &mut RenderTarget, area: Rect, g: &GitView, cat: &Catalog, t: &Theme) -> usize {
    let branches = match &g.branches {
        Load::Loading => {
            message(f, area, cat.git_loading_flow, t.overlay0);
            return 0;
        }
        Load::Error(e) => {
            message(f, area, &format!("{}: {e}", cat.git_error), t.coral);
            return 0;
        }
        Load::Loaded(v) if !v.is_empty() => v,
        _ => {
            message(f, area, cat.git_no_branches, t.overlay0);
            return 0;
        }
    };
    if area.height < 3 {
        return 0;
    }
    // Trunk = main / master / the checked-out branch.
    let trunk = branches
        .iter()
        .find(|b| b.name == "main")
        .or_else(|| branches.iter().find(|b| b.name == "master"))
        .or_else(|| branches.iter().find(|b| b.is_head))
        .map(|b| b.name.as_str())
        .unwrap_or("");
    let prs: &[PullRequest] = match &g.prs {
        Load::Loaded(v) => v,
        _ => &[],
    };

    // Build the chart rows; the legend is pinned, the chart scrolls above it.
    let track = (area.width.saturating_sub(34)).clamp(8, 40) as usize;
    let mut rows: Vec<Line> = vec![
        Line::from(vec![
            Span::styled(
                format!("⎇ {:<16}", trunc(trunk, 14)),
                Style::new().fg(t.accent).bold(),
            ),
            Span::styled("●", Style::new().fg(t.accent)),
            Span::styled("━".repeat(track), Style::new().fg(t.accent)),
            Span::styled("► ", Style::new().fg(t.accent)),
            Span::styled("merge", Style::new().fg(t.accent).bold()),
        ]),
        Line::from(Span::styled("  │", Style::new().fg(t.overlay0))),
    ];
    let lane = [t.mint, t.amber, t.coral, t.green, t.subtext1];
    for (i, b) in branches.iter().filter(|b| b.name != trunk).enumerate() {
        let col = lane[i % lane.len()];
        let track2 = b.ahead.clamp(1, 8) as usize;
        let dots = "●".repeat(track2);
        let mut spans = vec![
            Span::styled("  ╰─", Style::new().fg(t.overlay0)),
            Span::styled(
                format!("⎇ {:<16}", trunc(&b.name, 14)),
                Style::new().fg(col).bold(),
            ),
            Span::styled(dots, Style::new().fg(col)),
            Span::styled(
                "━".repeat(8usize.saturating_sub(track2)),
                Style::new().fg(t.surface1),
            ),
            Span::styled(
                format!("  ↑{} ↓{}", b.ahead, b.behind),
                Style::new().fg(t.subtext0),
            ),
        ];
        if let Some(pr) = prs.iter().find(|p| p.head == b.name) {
            let (badge, bcol) = pr_badge(pr, cat, t);
            spans.push(Span::styled(
                format!("   [{badge}] #{} ↗ merge", pr.number),
                Style::new().fg(bcol),
            ));
        }
        rows.push(Line::from(spans));
    }

    // Render the chart with the scroll offset; pin the legend to the last row.
    let legend_y = area.bottom().saturating_sub(1);
    let chart_h = legend_y.saturating_sub(area.y) as usize;
    let scroll = g.scroll.min(rows.len().saturating_sub(chart_h));
    for (y, line) in (area.y..).zip(rows.into_iter().skip(scroll).take(chart_h)) {
        f.render_widget(Paragraph::new(line), Rect::new(area.x, y, area.width, 1));
    }
    f.render_widget(
        Paragraph::new(Span::styled(
            format!(
                "  ● {}   ↑{} ↓{}   ↗ {} → {}",
                cat.flow_commit,
                cat.flow_ahead,
                cat.flow_behind,
                cat.flow_open_pr,
                cat.flow_merges_trunk
            ),
            Style::new().fg(t.overlay0),
        )),
        Rect::new(area.x, legend_y, area.width, 1),
    );
    scroll
}

fn draw_header(
    f: &mut RenderTarget,
    area: Rect,
    g: &GitView,
    cat: &Catalog,
    t: &Theme,
) -> Vec<(Section, Rect)> {
    let mut spans = vec![
        Span::styled(" ⎇ ", Style::new().fg(t.accent).bold()),
        Span::styled(g.repo_name.clone(), Style::new().fg(t.text).bold()),
    ];
    if let Load::Loaded(s) = &g.status {
        spans.push(Span::styled(
            format!("  {}", s.branch),
            Style::new().fg(t.accent),
        ));
        if let Some(up) = &s.upstream {
            spans.push(Span::styled(
                format!(" → {up}"),
                Style::new().fg(t.overlay0),
            ));
        }
        if s.ahead > 0 || s.behind > 0 {
            spans.push(Span::styled(
                format!("  ↑{} ↓{}", s.ahead, s.behind),
                Style::new().fg(t.subtext0),
            ));
        }
        let n = s.dirty_count();
        let (txt, col) = if n == 0 {
            ("· clean".to_string(), t.green)
        } else {
            (
                format!(
                    "· {n} {}",
                    if n == 1 {
                        cat.git_change
                    } else {
                        cat.git_changes
                    }
                ),
                t.amber,
            )
        };
        spans.push(Span::styled(format!("  {txt}"), Style::new().fg(col)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);

    // View selector, right-aligned — rendered per-tab so each is clickable.
    let labels: Vec<(Section, String)> = Section::ALL
        .iter()
        .map(|s| (*s, format!(" {} ", section_label(*s, cat))))
        .collect();
    let total: u16 = labels.iter().map(|(_, l)| l.chars().count() as u16).sum();
    let mut x = area.right().saturating_sub(total).max(area.x);
    let mut rects = Vec::with_capacity(labels.len());
    for (s, label) in labels {
        let w = label.chars().count() as u16;
        let style = if s == g.section {
            Style::new().fg(t.crust).bg(t.accent).bold()
        } else {
            Style::new().fg(t.subtext0)
        };
        let vis_w = w.min(area.right().saturating_sub(x));
        if vis_w > 0 {
            f.render_widget(
                Paragraph::new(Span::styled(label, style)),
                Rect::new(x, area.y, vis_w, 1),
            );
        }
        rects.push((s, Rect::new(x, area.y, w, 1)));
        x = x.saturating_add(w);
    }
    rects
}

fn draw_footer(f: &mut RenderTarget, area: Rect, g: &GitView, cat: &Catalog, t: &Theme) {
    if g.filtering {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" filter: ", Style::new().fg(t.subtext0)),
                Span::styled(g.filter.clone(), Style::new().fg(t.accent).bold()),
                Span::styled("▏", Style::new().fg(t.accent)),
            ])),
            area,
        );
        return;
    }
    // The PR detail panel owns the footer while it's open.
    if g.open_pr.is_some() {
        let pairs = [
            ("esc", cat.act_back),
            ("a", cat.act_approve),
            ("c", cat.act_checkout),
            ("o", cat.act_open),
            ("r", cat.act_refresh),
        ];
        f.render_widget(Paragraph::new(hint_line(&pairs, t)), area);
        return;
    }
    // The commit detail view owns the footer while it's open (docs/17).
    if g.open_commit.is_some() {
        let pairs = [
            ("esc", cat.act_back),
            ("j/k", cat.act_scroll),
            ("o", cat.act_open),
        ];
        f.render_widget(Paragraph::new(hint_line(&pairs, t)), area);
        return;
    }
    // The issue detail view owns the footer while it's open (docs/17).
    if g.open_issue.is_some() {
        let pairs = [
            ("esc", cat.act_back),
            ("j/k", cat.act_scroll),
            ("o", cat.act_open),
        ];
        f.render_widget(Paragraph::new(hint_line(&pairs, t)), area);
        return;
    }
    // Per-section hints as (key, label) pairs — the shared `hint_line` colors
    // the keys with the theme accent and the labels in light text.
    let scope = scope_label(g.scope, cat);
    // Current state filter, shown on the `s` hint (like `m` shows scope). Issues
    // display "merged" as "all" since they can't be merged.
    let state = if g.section == Section::Issues {
        g.state_filter.issue_arg()
    } else {
        g.state_filter.gh_arg()
    };
    let pairs: Vec<(&str, &str)> = match g.section {
        Section::Prs => vec![
            ("j/k", cat.act_move),
            ("⏎", cat.act_details),
            ("o", cat.act_open),
            ("s", state),
            ("m", scope),
            ("c", cat.act_new),
            ("/", cat.act_filter),
            ("q", cat.act_close),
        ],
        Section::Issues => vec![
            ("j/k", cat.act_move),
            ("⏎", cat.act_view),
            ("o", cat.act_open),
            ("s", state),
            ("m", scope),
            ("/", cat.act_filter),
            ("q", cat.act_close),
        ],
        Section::Branches => vec![
            ("j/k", cat.act_move),
            ("⏎", cat.act_checkout),
            ("d", cat.act_log),
            ("/", cat.act_filter),
            ("click", cat.act_tab),
            ("r", cat.act_refresh),
            ("q", cat.act_close),
        ],
        Section::Commits => vec![
            ("j/k", cat.act_move),
            ("⏎", cat.act_show),
            ("/", cat.act_filter),
            ("click", cat.act_tab),
            ("r", cat.act_refresh),
            ("q", cat.act_close),
        ],
        Section::Flow => vec![
            ("j/k", cat.act_scroll),
            ("click", cat.act_tab),
            ("r", cat.act_refresh),
            ("q", cat.act_close),
        ],
        // Status owns the contributor list, so it advertises the expand toggle.
        // The `x` email toggle is deliberately left off this line.
        Section::Status => vec![
            ("j/k", cat.act_scroll),
            ("↑/↓", cat.act_select),
            ("⏎", cat.act_diff),
            ("E", cat.st_show_more),
            ("r", cat.act_refresh),
            ("q", cat.act_close),
        ],
    };
    f.render_widget(Paragraph::new(hint_line(&pairs, t)), area);
}

fn draw_branches(f: &mut RenderTarget, area: Rect, g: &GitView, cat: &Catalog, t: &Theme) {
    let v = match &g.branches {
        Load::Loading => return message(f, area, cat.git_loading_branches, t.overlay0),
        Load::Error(e) => return message(f, area, &format!("{}: {e}", cat.git_error), t.coral),
        Load::Loaded(v) => v,
        Load::Idle => return,
    };
    let sub_w = area.width.saturating_sub(50).max(10);
    let rows: Vec<Line> = filtered_branches(v, &g.filter)
        .map(|b| {
            let name_style = if b.is_head {
                Style::new().fg(t.green).bold()
            } else {
                Style::new().fg(t.accent)
            };
            let track = if b.ahead > 0 || b.behind > 0 {
                format!("↑{} ↓{}", b.ahead, b.behind)
            } else {
                String::new()
            };
            Line::from(vec![
                Span::styled(
                    if b.is_head { "● " } else { "  " },
                    Style::new().fg(t.green),
                ),
                Span::styled(pad(&b.name, 22), name_style),
                Span::styled(pad(&track, 8), Style::new().fg(t.subtext0)),
                Span::styled(pad(&b.subject, sub_w as usize), Style::new().fg(t.text)),
                Span::styled(
                    format!("{} · {}", trunc(&b.author, 12), b.when),
                    Style::new().fg(t.overlay0),
                ),
            ])
        })
        .collect();
    draw_list(f, area, rows, g.cursor, cat, t);
}

fn draw_commits(f: &mut RenderTarget, area: Rect, g: &GitView, cat: &Catalog, t: &Theme) {
    let v = match &g.commits {
        Load::Loading => return message(f, area, cat.git_loading_commits, t.overlay0),
        Load::Error(e) => return message(f, area, &format!("{}: {e}", cat.git_error), t.coral),
        Load::Loaded(v) => v,
        Load::Idle => return,
    };
    // Match PRs and Issues: a fluid main column plus stable metadata columns.
    // Reserving one graph width and refs column across all rows makes the list
    // scan as a table instead of allowing each row to drift.
    let has_refs = v.iter().any(|c| !c.refs.is_empty());
    let graph_width = v
        .iter()
        .map(|c| crate::ui::display_width(&c.graph))
        .max()
        .unwrap_or(0);
    let rows: Vec<Line> = filtered_commits(v, &g.filter)
        .map(|c| {
            commit_row(
                c,
                area.width.saturating_sub(2) as usize,
                graph_width,
                has_refs,
                t,
            )
        })
        .collect();
    draw_list(f, area, rows, g.cursor, cat, t);
}

/// Commit table widths. The subject uses all remaining width, like PR/Issue
/// titles, while refs, author, and relative time remain readable columns.
fn commit_columns(row_width: usize, graph_width: usize, has_refs: bool) -> (usize, usize, usize) {
    const SHA_WIDTH: usize = 8; // seven-char SHA plus one separating space
    const REFS_WIDTH: usize = 30;
    const META_WIDTH: usize = 28; // author (12) + separator + relative time
    const SUBJECT_MIN: usize = 12;

    let available = row_width.saturating_sub(graph_width + SHA_WIDTH);
    let meta = if available >= SUBJECT_MIN + META_WIDTH {
        META_WIDTH
    } else {
        0
    };
    let refs = if has_refs && available >= SUBJECT_MIN + meta + REFS_WIDTH {
        REFS_WIDTH
    } else {
        0
    };
    let subject = available.saturating_sub(refs + meta).max(SUBJECT_MIN);
    (subject, refs, meta)
}

fn commit_row(
    c: &crate::git::model::Commit,
    row_width: usize,
    graph_width: usize,
    has_refs: bool,
    t: &Theme,
) -> Line<'static> {
    let (subject_w, refs_w, meta_w) = commit_columns(row_width, graph_width, has_refs);
    let mut spans = Vec::with_capacity(7);
    if graph_width > 0 {
        spans.push(Span::styled(
            pad(&c.graph, graph_width),
            Style::new().fg(t.overlay0),
        ));
    }
    spans.push(Span::styled(pad(&c.sha, 8), Style::new().fg(t.amber)));
    spans.push(Span::styled(
        pad(&c.subject, subject_w),
        Style::new().fg(t.text),
    ));
    if refs_w > 0 {
        spans.push(Span::styled(pad(&c.refs, refs_w), Style::new().fg(t.mint)));
    }
    if meta_w > 0 {
        let when_w = 12.min(meta_w.saturating_sub(4));
        let author_w = meta_w.saturating_sub(when_w + 3);
        spans.push(Span::styled(
            pad(&c.author, author_w),
            Style::new().fg(t.subtext0),
        ));
        spans.push(Span::styled(" · ", Style::new().fg(t.surface1)));
        spans.push(Span::styled(
            trunc(&c.when, when_w),
            Style::new().fg(t.overlay0),
        ));
    }
    Line::from(spans)
}

/// Returns the clamped scroll offset and, when it is on screen, the rect of the
/// contributors "show more / show less" row so a click can toggle it.
fn draw_status(
    f: &mut RenderTarget,
    area: Rect,
    g: &mut GitView,
    cat: &Catalog,
    t: &Theme,
) -> (usize, Option<Rect>) {
    let s = match &g.status {
        Load::Loading => {
            message(f, area, cat.git_loading_status, t.overlay0);
            return (0, None);
        }
        Load::Error(e) => {
            message(f, area, &format!("{}: {e}", cat.git_error), t.coral);
            return (0, None);
        }
        Load::Loaded(s) => s,
        Load::Idle => return (0, None),
    };
    let mut rows: Vec<Line> = Vec::new();
    // Index of the contributors "show more / show less" row, if it was drawn.
    let mut more_row: Option<usize> = None;
    let header = |rows: &mut Vec<Line>, title: String| {
        rows.push(Line::from(Span::styled(
            title,
            Style::new().fg(t.subtext1).bold(),
        )));
    };
    let group = |rows: &mut Vec<Line>, title: String, items: Vec<Line<'static>>| {
        if items.is_empty() {
            return;
        }
        header(rows, title);
        rows.extend(items);
        rows.push(Line::from(""));
    };

    // ── Repository overview (from local git, no `gh` needed) ──
    match &g.info {
        Load::Loaded(info) => {
            header(&mut rows, cat.st_repository.to_string());
            if let Some(slug) = &info.slug {
                rows.push(Line::from(vec![
                    Span::raw("   "),
                    Span::styled(slug.clone(), Style::new().fg(t.accent).bold()),
                    Span::styled(
                        format!("  {}", info.host.as_deref().unwrap_or("")),
                        Style::new().fg(t.overlay0),
                    ),
                ]));
            }
            let url = info.remote_url.as_deref().unwrap_or("(no remote)");
            rows.push(Line::from(Span::styled(
                format!("   {url}"),
                Style::new().fg(t.subtext0),
            )));
            let mut stats = format!("{} commits", info.total_commits);
            if let Some(age) = &info.age {
                stats.push_str(&format!(" · started {age}"));
            }
            if !info.contributors.is_empty() {
                stats.push_str(&format!(" · {} contributors", info.contributors.len()));
            }
            rows.push(Line::from(Span::styled(
                format!("   {stats}"),
                Style::new().fg(t.subtext0),
            )));
            rows.push(Line::from(""));

            if !info.contributors.is_empty() {
                header(&mut rows, cat.st_contributors.to_string());
                let top = info
                    .contributors
                    .first()
                    .map(|c| c.commits)
                    .unwrap_or(1)
                    .max(1);
                let (shown, hidden) =
                    crate::git::visible_contributors(&info.contributors, g.contributors_expanded);
                // Emails are hidden by default (privacy — the list is about who
                // contributes, not how to mail them); `x` reveals them. With them
                // hidden the name gets that width back, so long handles stay whole.
                let name_w = if g.show_emails { 18 } else { 34 };
                for c in shown {
                    let bar = (c.commits as usize * 12 / top as usize).max(1);
                    let mut spans = vec![
                        Span::styled(
                            format!("   {}", pad(&c.name, name_w)),
                            Style::new().fg(t.text),
                        ),
                        Span::styled(format!("{:>4}  ", c.commits), Style::new().fg(t.accent)),
                        Span::styled("█".repeat(bar), Style::new().fg(t.green)),
                    ];
                    if g.show_emails {
                        spans.push(Span::styled(
                            format!("  {}", trunc(&c.email, 26)),
                            Style::new().fg(t.overlay0),
                        ));
                    }
                    rows.push(Line::from(spans));
                }
                // The show more / show less row is clickable — remember which row
                // it is so the render loop below can map it to a screen rect.
                if hidden > 0 {
                    more_row = Some(rows.len());
                    rows.push(Line::from(Span::styled(
                        format!("   ↓ +{hidden}  {}", cat.st_show_more),
                        Style::new().fg(t.accent),
                    )));
                } else if g.contributors_expanded && info.contributors.len() > 1 {
                    more_row = Some(rows.len());
                    rows.push(Line::from(Span::styled(
                        format!("   ↑  {}", cat.st_show_less),
                        Style::new().fg(t.accent),
                    )));
                }
                rows.push(Line::from(""));
            }
        }
        Load::Loading => {
            rows.push(Line::from(Span::styled(
                "Repository  loading…",
                Style::new().fg(t.overlay0),
            )));
            rows.push(Line::from(""));
        }
        _ => {}
    }

    // ── Working tree ──
    let clean = s.dirty_count() == 0 && s.stashes.is_empty();
    // Track staged/unstaged file row indices for Enter/d hit-testing.
    if !s.staged.is_empty() {
        header(&mut rows, format!("{} ({})", cat.st_staged, s.staged.len()));
        let start = rows.len();
        rows.extend(
            s.staged
                .iter()
                .map(|c| file_line(c.code, &c.path, t.green, t)),
        );
        g.status_staged_rows = start..rows.len();
        rows.push(Line::from(""));
    } else {
        g.status_staged_rows = 0..0;
    }
    if !s.unstaged.is_empty() {
        header(
            &mut rows,
            format!("{} ({})", cat.st_changed, s.unstaged.len()),
        );
        let start = rows.len();
        rows.extend(
            s.unstaged
                .iter()
                .map(|c| file_line(c.code, &c.path, t.amber, t)),
        );
        g.status_unstaged_rows = start..rows.len();
        rows.push(Line::from(""));
    } else {
        g.status_unstaged_rows = 0..0;
    }
    group(
        &mut rows,
        format!("{} ({})", cat.st_untracked, s.untracked.len()),
        s.untracked
            .iter()
            .map(|p| file_line('?', p, t.overlay1, t))
            .collect(),
    );
    group(
        &mut rows,
        format!("{} ({})", cat.st_stashes, s.stashes.len()),
        s.stashes
            .iter()
            .map(|p| Line::from(Span::styled(format!("   {p}"), Style::new().fg(t.subtext0))))
            .collect(),
    );
    if clean {
        header(&mut rows, cat.st_working_tree.to_string());
        rows.push(Line::from(Span::styled(
            format!("   {} ✓", cat.st_clean),
            Style::new().fg(t.green),
        )));
    }

    // Status has an explicit file selection, while repository metadata remains
    // independently scrollable with j/k.
    let avail = area.height as usize;
    let scroll = g.scroll.min(rows.len().saturating_sub(avail));
    let selected_row = g.status_selected.as_ref().and_then(|(path, staged)| {
        let changes = if *staged { &s.staged } else { &s.unstaged };
        let rows = if *staged {
            &g.status_staged_rows
        } else {
            &g.status_unstaged_rows
        };
        changes
            .iter()
            .position(|change| change.path == *path)
            .map(|index| rows.start + index)
    });
    for (index, line) in rows.into_iter().enumerate().skip(scroll).take(avail) {
        let y = area.y + (index - scroll) as u16;
        let row = Rect::new(area.x, y, area.width, 1);
        if selected_row == Some(index) {
            fill_bg(f, row, t.sel_bg);
        }
        f.render_widget(Paragraph::new(line), row);
    }
    // Map the toggle row to a screen rect, but only while it is actually visible
    // in this frame's scroll window — a stale rect would fire from empty space.
    let more_rect = more_row
        .filter(|i| *i >= scroll && *i < scroll + avail)
        .map(|i| Rect::new(area.x, area.y + (i - scroll) as u16, area.width, 1));
    (scroll, more_rect)
}

fn file_line(code: char, path: &str, code_color: Color, t: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("   {code}  "), Style::new().fg(code_color).bold()),
        Span::styled(path.to_string(), Style::new().fg(t.text)),
    ])
}

/// A scrolling, cursor-highlighted list.
fn draw_list(
    f: &mut RenderTarget,
    area: Rect,
    rows: Vec<Line<'static>>,
    cursor: usize,
    cat: &Catalog,
    t: &Theme,
) {
    if rows.is_empty() {
        return message(f, area, cat.git_nothing_here, t.overlay0);
    }
    let avail = area.height as usize;
    if avail == 0 {
        return;
    }
    let cursor = cursor.min(rows.len().saturating_sub(1));
    let scroll = cursor.saturating_sub(avail.saturating_sub(1));
    for (i, line) in rows.into_iter().enumerate().skip(scroll).take(avail) {
        let ry = area.y + (i - scroll) as u16;
        let sel = i == cursor;
        let row = Rect::new(area.x, ry, area.width, 1);
        if sel {
            fill_bg(f, row, t.sel_bg);
        }
        f.render_widget(
            Paragraph::new(Span::styled(
                if sel { "»" } else { " " },
                Style::new().fg(t.accent).bold(),
            )),
            Rect::new(area.x, ry, 1, 1),
        );
        f.render_widget(
            Paragraph::new(line),
            Rect::new(area.x + 2, ry, area.width.saturating_sub(2), 1),
        );
    }
}

#[cfg(test)]
mod detail_layout_tests {
    use super::*;
    use crate::git::model::{Check, DiscussionComment, Review};
    use ratatui::buffer::Buffer;

    fn find_text(buffer: &Buffer, needle: &str) -> Option<(u16, u16)> {
        for y in buffer.area.y..buffer.area.bottom() {
            let row = (buffer.area.x..buffer.area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>();
            if let Some(column) = row.find(needle) {
                return Some((column as u16, y));
            }
        }
        None
    }

    fn comment() -> DiscussionComment {
        DiscussionComment {
            author: "reviewer".into(),
            body: "This comment belongs in the discussion column.".into(),
            created_at: "2026-09-02T08:00:00Z".into(),
        }
    }

    fn pr_detail() -> PrDetail {
        PrDetail {
            number: 42,
            title: "Improve Git detail UX".into(),
            state: "OPEN".into(),
            is_draft: false,
            author: "alice".into(),
            base: "main".into(),
            head: "feature/git-reader".into(),
            body: "The pull request description stays in the reading column.".into(),
            additions: 120,
            deletions: 8,
            changed_files: 5,
            commits: 3,
            comment_count: 1,
            comments: vec![comment()],
            mergeable: "MERGEABLE".into(),
            review_decision: "APPROVED".into(),
            reviews: vec![Review {
                author: "bob".into(),
                state: "APPROVED".into(),
            }],
            check_runs: vec![Check {
                name: "build".into(),
                bucket: Checks::Passing,
            }],
            labels: vec!["ux".into()],
            updated_at: "2026-09-02T09:00:00Z".into(),
        }
    }

    #[test]
    fn wide_pr_detail_keeps_discussion_left_and_metadata_right() {
        let area = Rect::new(0, 0, 120, 30);
        let (main, divider, rail) = detail_columns(area).expect("wide split");
        assert_eq!(main.width, 78);
        assert_eq!(divider, 78);
        assert_eq!(rail, Rect::new(80, 0, 40, 30));

        let mut view = GitView::new(std::path::PathBuf::from("."));
        view.detail = Load::Loaded(pr_detail());
        let mut buffer = Buffer::empty(area);
        let mut target = RenderTarget::new(&mut buffer, area);
        draw_pr_detail(
            &mut target,
            area,
            &mut view,
            crate::i18n::by_code("en"),
            &Theme::quattro_rally(),
        );

        assert!(find_text(&buffer, "pull request description").unwrap().0 < divider);
        assert!(find_text(&buffer, "This comment belongs").unwrap().0 < divider);
        assert!(find_text(&buffer, "feature/git-reader").unwrap().0 >= rail.x);
        assert!(find_text(&buffer, "Checks").unwrap().0 >= rail.x);
        assert!(find_text(&buffer, "● ux").unwrap().0 >= rail.x);
    }

    #[test]
    fn wide_issue_detail_shows_comments_and_ownership_rail() {
        let area = Rect::new(0, 0, 120, 24);
        let mut view = GitView::new(std::path::PathBuf::from("."));
        view.issue_detail = Load::Loaded(IssueDetail {
            number: 7,
            title: "Improve issue details".into(),
            state: "OPEN".into(),
            author: "alice".into(),
            body: "Issue description in the main column.".into(),
            labels: vec!["bug".into()],
            assignees: vec!["bob".into()],
            comment_count: 1,
            comments: vec![comment()],
            updated_at: "2026-09-02T09:00:00Z".into(),
        });
        let mut buffer = Buffer::empty(area);
        let mut target = RenderTarget::new(&mut buffer, area);
        draw_issue_detail(
            &mut target,
            area,
            &mut view,
            crate::i18n::by_code("en"),
            &Theme::quattro_rally(),
        );

        let (_, divider, rail) = detail_columns(area).unwrap();
        assert!(find_text(&buffer, "Issue description").unwrap().0 < divider);
        assert!(find_text(&buffer, "This comment belongs").unwrap().0 < divider);
        assert!(find_text(&buffer, "● bug").unwrap().0 >= rail.x);
        assert!(find_text(&buffer, "@bob").unwrap().0 >= rail.x);
    }

    #[test]
    fn narrow_detail_stacks_metadata_without_hiding_the_discussion() {
        let area = Rect::new(0, 0, 70, 40);
        assert!(detail_columns(area).is_none());
        let mut view = GitView::new(std::path::PathBuf::from("."));
        view.detail = Load::Loaded(pr_detail());
        let mut buffer = Buffer::empty(area);
        let mut target = RenderTarget::new(&mut buffer, area);
        draw_pr_detail(
            &mut target,
            area,
            &mut view,
            crate::i18n::by_code("en"),
            &Theme::quattro_rally(),
        );

        assert!(find_text(&buffer, "feature/git-reader").is_some());
        assert!(find_text(&buffer, "pull request description").is_some());
        assert!(find_text(&buffer, "This comment belongs").is_some());
    }

    #[test]
    fn discussion_rows_are_reused_until_target_or_width_changes() {
        let comments = vec![comment()];
        let mut cache = None;
        assert!(update_detail_text_cache(
            &mut cache,
            DetailTextTarget::Pr(42),
            78,
            "description",
            &comments,
            1,
        ));
        let rows = cache.as_ref().unwrap().rows.as_ptr();

        assert!(!update_detail_text_cache(
            &mut cache,
            DetailTextTarget::Pr(42),
            78,
            "description",
            &comments,
            1,
        ));
        assert_eq!(cache.as_ref().unwrap().rows.as_ptr(), rows);

        assert!(update_detail_text_cache(
            &mut cache,
            DetailTextTarget::Pr(42),
            60,
            "description",
            &comments,
            1,
        ));
        assert_eq!(cache.as_ref().unwrap().width, 60);
    }

    #[test]
    fn long_unbroken_comment_is_split_to_the_reader_width() {
        let comments = vec![DiscussionComment {
            author: "reviewer".into(),
            body: "https://example.com/abcdefghijklmnopqrstuvwxyz0123456789".into(),
            created_at: "2026-09-02T08:00:00Z".into(),
        }];
        let rows = DetailTextCache::rows(20, "description", &comments, 1, wrap);
        let body_rows: Vec<&str> = rows
            .iter()
            .filter_map(|row| match row {
                DetailTextRow::CommentBody(line) => Some(line.as_str()),
                _ => None,
            })
            .collect();

        assert!(body_rows.len() > 1);
        assert!(body_rows.iter().all(|line| line.chars().count() <= 17));
        assert_eq!(body_rows.concat(), comments[0].body);
    }

    #[test]
    fn wide_short_details_can_scroll_to_late_metadata() {
        let area = Rect::new(0, 0, 120, 7);
        let mut pr = GitView::new(std::path::PathBuf::from("."));
        pr.detail = Load::Loaded(pr_detail());
        pr.scroll = usize::MAX;
        let mut pr_buffer = Buffer::empty(area);
        let mut pr_target = RenderTarget::new(&mut pr_buffer, area);
        draw_pr_detail(
            &mut pr_target,
            area,
            &mut pr,
            crate::i18n::by_code("en"),
            &Theme::quattro_rally(),
        );
        assert!(find_text(&pr_buffer, "bob").is_some());

        let mut issue = GitView::new(std::path::PathBuf::from("."));
        issue.issue_detail = Load::Loaded(IssueDetail {
            number: 7,
            title: "Improve issue details".into(),
            state: "OPEN".into(),
            author: "alice".into(),
            body: "Issue description in the main column.".into(),
            labels: vec!["bug".into(), "ux".into(), "reader".into()],
            assignees: vec!["bob".into()],
            comment_count: 1,
            comments: vec![comment()],
            updated_at: "2026-09-02T09:00:00Z".into(),
        });
        issue.scroll = usize::MAX;
        let mut issue_buffer = Buffer::empty(area);
        let mut issue_target = RenderTarget::new(&mut issue_buffer, area);
        draw_issue_detail(
            &mut issue_target,
            area,
            &mut issue,
            crate::i18n::by_code("en"),
            &Theme::quattro_rally(),
        );
        assert!(find_text(&issue_buffer, "@bob").is_some());
    }
}

fn message(f: &mut RenderTarget, area: Rect, text: &str, color: Color) {
    if area.height == 0 {
        return;
    }
    f.render_widget(
        Paragraph::new(Span::styled(format!("  {text}"), Style::new().fg(color))),
        Rect::new(area.x, area.y, area.width, 1),
    );
}

fn hline(f: &mut RenderTarget, x: u16, y: u16, w: u16, t: &Theme) {
    let buf = f.buffer_mut();
    for i in 0..w {
        buf[(x + i, y)]
            .set_symbol("─")
            .set_style(Style::new().fg(t.surface1).bg(t.mantle));
    }
}

fn fill_bg(f: &mut RenderTarget, rect: Rect, color: Color) {
    let buf = f.buffer_mut();
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            buf[(x, y)].set_bg(color);
        }
    }
}

/// Truncate to `n` display chars with an ellipsis.
fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else if n <= 1 {
        "…".to_string()
    } else {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    }
}

/// Truncate then pad to exactly `n` columns.
fn pad(s: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    let s = trunc(s, n);
    format!("{s:<n$}")
}

#[cfg(test)]
mod tests {
    use super::{commit_columns, pad, pr_title_width};

    #[test]
    fn padded_columns_never_exceed_their_declared_width() {
        assert_eq!(pad("a", 3), "a  ");
        assert_eq!(pad("abc", 3), "abc");
        assert_eq!(pad("abcd", 3), "ab…");
        assert_eq!(pad("anything", 0), "");
    }

    #[test]
    fn pr_rows_reserve_fixed_columns_before_the_title() {
        assert_eq!(pr_title_width(140), 75);
        assert_eq!(pr_title_width(50), 12);
    }

    #[test]
    fn wide_commit_rows_fill_the_table_before_metadata() {
        // Like PR/Issue titles, the subject receives every column left after
        // the graph, refs, author, and time fields have been reserved.
        assert_eq!(commit_columns(180, 2, true), (112, 30, 28));
    }

    #[test]
    fn narrow_commit_rows_prioritize_the_subject() {
        // On small displays references yield first; metadata is omitted only
        // once it would crowd out the commit message itself.
        assert_eq!(commit_columns(58, 2, true), (20, 0, 28));
        assert_eq!(commit_columns(30, 2, true), (20, 0, 0));
    }
}
