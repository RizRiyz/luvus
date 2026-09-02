//! Human-facing CLI localization.
//!
//! Command names, flags, JSON, UHP, error codes, IDs, paths, and user data are
//! canonical. Only prose from known Luvus-owned strings is translated. Catalogs
//! are compiled into the binary and selected once per CLI invocation.

use std::borrow::Cow;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Language {
    En,
    Es,
    Pt,
    Fr,
    De,
    Id,
    Zh,
    Ja,
    Ko,
}

#[derive(Clone, Copy, Debug)]
pub struct Context {
    language: Language,
}

impl Context {
    pub const fn for_language(language: Language) -> Self {
        Self { language }
    }

    /// Resolve the selected Luvus language with one read-only config load.
    pub fn configured() -> Self {
        Self::for_language(Language::from_code(&crate::config::load().language))
    }

    pub fn language(self) -> Language {
        self.language
    }

    pub fn text(self, english: &'static str) -> &'static str {
        text(english, self.language)
    }

    /// Translate a complete Luvus-owned message, then substitute canonical
    /// values such as command names, versions, paths, and agent identifiers.
    pub fn render(self, english: &'static str, values: &[(&str, &str)]) -> String {
        let mut rendered = self.text(english).to_string();
        for (name, value) in values {
            rendered = rendered.replace(&format!("{{{name}}}"), value);
        }
        rendered
    }
}

impl Language {
    pub fn from_code(code: &str) -> Self {
        match code {
            "es" => Self::Es,
            "pt" => Self::Pt,
            "fr" => Self::Fr,
            "de" => Self::De,
            "id" => Self::Id,
            "zh" => Self::Zh,
            "ja" => Self::Ja,
            "ko" => Self::Ko,
            _ => Self::En,
        }
    }

    #[cfg(test)]
    pub fn code(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Es => "es",
            Self::Pt => "pt",
            Self::Fr => "fr",
            Self::De => "de",
            Self::Id => "id",
            Self::Zh => "zh",
            Self::Ja => "ja",
            Self::Ko => "ko",
        }
    }
}

struct Translation {
    en: &'static str,
    es: &'static str,
    pt: &'static str,
    fr: &'static str,
    de: &'static str,
    id: &'static str,
    zh: &'static str,
    ja: &'static str,
    ko: &'static str,
}

impl Translation {
    fn get(&self, language: Language) -> &'static str {
        match language {
            Language::En => self.en,
            Language::Es => self.es,
            Language::Pt => self.pt,
            Language::Fr => self.fr,
            Language::De => self.de,
            Language::Id => self.id,
            Language::Zh => self.zh,
            Language::Ja => self.ja,
            Language::Ko => self.ko,
        }
    }
}

macro_rules! tr {
    ($en:literal, $es:literal, $pt:literal, $fr:literal, $de:literal, $id:literal, $zh:literal, $ja:literal, $ko:literal) => {
        Translation {
            en: $en,
            es: $es,
            pt: $pt,
            fr: $fr,
            de: $de,
            id: $id,
            zh: $zh,
            ja: $ja,
            ko: $ko,
        }
    };
}

/// Complete prose catalog used by compact, full, topic, and leaf help.
/// Entries are matched only as a complete line or a whitespace-delimited line
/// suffix, so canonical syntax at the start of a row is never rewritten.
static HELP: &[Translation] = &[
    tr!(
        "expose scoped UHP through a private provider endpoint",
        "exponer UHP con alcance mediante un endpoint privado para proveedores",
        "expor UHP com escopo por um endpoint privado para provedores",
        "exposer UHP avec une portée limitée via un endpoint privé pour fournisseurs",
        "UHP mit begrenztem Umfang über einen privaten Anbieter-Endpunkt bereitstellen",
        "ekspos UHP terbatas melalui endpoint penyedia privat",
        "通过私有提供方端点公开限定范围的 UHP",
        "スコープを限定した UHP をプライベートなプロバイダー用エンドポイントで公開",
        "범위가 제한된 UHP를 비공개 공급자 엔드포인트로 노출"
    ),
    tr!(
        "Read local structured server and client diagnostics",
        "Leer diagnósticos estructurados locales del servidor y cliente",
        "Ler diagnósticos estruturados locais do servidor e cliente",
        "Lire les diagnostics structurés locaux du serveur et du client",
        "Lokale strukturierte Server- und Clientdiagnosen lesen",
        "Baca diagnostik terstruktur server dan klien lokal",
        "读取本地结构化服务器和客户端诊断",
        "ローカルの構造化サーバー・クライアント診断を読み取る",
        "로컬의 구조화된 서버 및 클라이언트 진단을 읽습니다"
    ),
    tr!(
        "read local structured diagnostics",
        "leer diagnósticos estructurados locales",
        "ler diagnósticos estruturados locais",
        "lire les diagnostics structurés locaux",
        "lokale strukturierte Diagnosen lesen",
        "baca diagnostik terstruktur lokal",
        "读取本地结构化诊断",
        "ローカルの構造化診断を読み取る",
        "로컬 구조화된 진단 읽기"
    ),
    tr!(
        "Read local structured runtime diagnostics without starting the server.",
        "Leer diagnósticos estructurados locales del entorno sin iniciar el servidor.",
        "Ler diagnósticos estruturados locais do runtime sem iniciar o servidor.",
        "Lire les diagnostics structurés locaux sans démarrer le serveur.",
        "Lokale strukturierte Laufzeitdiagnosen lesen, ohne den Server zu starten.",
        "Baca diagnostik runtime terstruktur lokal tanpa memulai server.",
        "无需启动服务器即可读取本地结构化运行时诊断。",
        "サーバーを起動せずにローカルの構造化ランタイム診断を読み取ります。",
        "서버를 시작하지 않고 로컬 구조화된 런타임 진단을 읽습니다."
    ),
    tr!(
        "luvus: Mission control for your AI coding agents",
        "luvus: Centro de control para tus agentes de programación con IA",
        "luvus: Central de controle para seus agentes de programação com IA",
        "luvus : Centre de contrôle pour vos agents de programmation IA",
        "luvus: Kontrollzentrum für deine KI-Programmieragenten",
        "luvus: Pusat kendali untuk agen pemrograman AI Anda",
        "luvus：AI 编程智能体的任务控制中心",
        "luvus：AI コーディングエージェントのミッションコントロール",
        "luvus: AI 코딩 에이전트를 위한 미션 컨트롤"
    ),
    tr!(
        "Usage:",
        "Uso:",
        "Uso:",
        "Utilisation :",
        "Verwendung:",
        "Penggunaan:",
        "用法：",
        "使用法：",
        "사용법:"
    ),
    tr!(
        "usage:",
        "uso:",
        "uso:",
        "utilisation :",
        "verwendung:",
        "penggunaan:",
        "用法：",
        "使用法：",
        "사용법:"
    ),
    tr!(
        "Commands:",
        "Comandos:",
        "Comandos:",
        "Commandes :",
        "Befehle:",
        "Perintah:",
        "命令：",
        "コマンド：",
        "명령어:"
    ),
    tr!(
        "Examples:",
        "Ejemplos:",
        "Exemplos:",
        "Exemples :",
        "Beispiele:",
        "Contoh:",
        "示例：",
        "例：",
        "예시:"
    ),
    tr!(
        "Options:",
        "Opciones:",
        "Opções:",
        "Options :",
        "Optionen:",
        "Opsi:",
        "选项：",
        "オプション：",
        "옵션:"
    ),
    tr!(
        "Help:",
        "Ayuda:",
        "Ajuda:",
        "Aide :",
        "Hilfe:",
        "Bantuan:",
        "帮助：",
        "ヘルプ：",
        "도움말:"
    ),
    tr!(
        "workspaces:",
        "espacios de trabajo:",
        "espaços de trabalho:",
        "espaces de travail :",
        "Arbeitsbereiche:",
        "ruang kerja:",
        "工作区：",
        "ワークスペース：",
        "작업 공간:"
    ),
    tr!(
        "tabs:",
        "pestañas:",
        "abas:",
        "onglets :",
        "Tabs:",
        "tab:",
        "标签页：",
        "タブ：",
        "탭:"
    ),
    tr!(
        "panes / agents:",
        "paneles / agentes:",
        "painéis / agentes:",
        "volets / agents :",
        "Bereiche / Agenten:",
        "panel / agen:",
        "窗格 / 智能体：",
        "ペイン / エージェント：",
        "패널 / 에이전트:"
    ),
    tr!(
        "search:",
        "búsqueda:",
        "pesquisa:",
        "recherche :",
        "Suche:",
        "pencarian:",
        "搜索：",
        "検索：",
        "검색:"
    ),
    tr!(
        "themes:",
        "temas:",
        "temas:",
        "thèmes :",
        "Themes:",
        "tema:",
        "主题：",
        "テーマ：",
        "테마:"
    ),
    tr!(
        "bars:",
        "barras:",
        "barras:",
        "barres :",
        "Leisten:",
        "bar:",
        "状态栏：",
        "バー：",
        "바:"
    ),
    tr!(
        "appearance:",
        "apariencia:",
        "aparência:",
        "apparence :",
        "Darstellung:",
        "tampilan:",
        "外观：",
        "外観：",
        "모양:"
    ),
    tr!(
        "modules (extensions):",
        "módulos (extensiones):",
        "módulos (extensões):",
        "modules (extensions) :",
        "Module (Erweiterungen):",
        "modul (ekstensi):",
        "模块（扩展）：",
        "モジュール（拡張）：",
        "모듈 (확장 기능):"
    ),
    tr!("git:", "git:", "git:", "git :", "Git:", "git:", "Git：", "Git：", "git:"),
    tr!(
        "mission control:",
        "control de misión:",
        "controle de missão:",
        "contrôle de mission :",
        "Missionskontrolle:",
        "kontrol misi:",
        "任务控制：",
        "ミッションコントロール：",
        "미션 컨트롤:"
    ),
    tr!(
        "diff review:",
        "revisión de diff:",
        "revisão de diff:",
        "revue des différences :",
        "Diff-Prüfung:",
        "tinjauan diff:",
        "差异审查：",
        "差分レビュー：",
        "diff 검토:"
    ),
    tr!(
        "worktrees:",
        "árboles de trabajo:",
        "árvores de trabalho:",
        "arbres de travail :",
        "Worktrees:",
        "worktree:",
        "工作树：",
        "ワークツリー：",
        "worktree:"
    ),
    tr!(
        "orchestration (multiple agents on one project, docs/22):",
        "orquestación (varios agentes en un proyecto, docs/22):",
        "orquestração (vários agentes em um projeto, docs/22):",
        "orchestration (plusieurs agents sur un projet, docs/22) :",
        "Orchestrierung (mehrere Agenten in einem Projekt, docs/22):",
        "orkestrasi (beberapa agen dalam satu proyek, docs/22):",
        "编排（一个项目中的多个智能体，docs/22）：",
        "オーケストレーション（1つのプロジェクトの複数エージェント、docs/22）：",
        "오케스트레이션 (한 프로젝트에서 여러 에이전트 운용, docs/22):"
    ),
    tr!(
        "events:",
        "eventos:",
        "eventos:",
        "événements :",
        "Ereignisse:",
        "peristiwa:",
        "事件：",
        "イベント：",
        "이벤트:"
    ),
    tr!(
        "universal harness protocol:",
        "protocolo universal de arneses:",
        "protocolo universal de harness:",
        "protocole universel de harness :",
        "Universal Harness Protocol:",
        "protokol harness universal:",
        "通用智能体框架协议：",
        "Universal Harness Protocol：",
        "universal harness protocol:"
    ),
    tr!(
        "sessions:",
        "sesiones:",
        "sessões:",
        "sessions :",
        "Sitzungen:",
        "sesi:",
        "会话：",
        "セッション：",
        "세션:"
    ),
    tr!(
        "remote:",
        "remoto:",
        "remoto:",
        "distant :",
        "Remote:",
        "jarak jauh:",
        "远程：",
        "リモート：",
        "원격:"
    ),
    tr!(
        "server:",
        "servidor:",
        "servidor:",
        "serveur :",
        "Server:",
        "server:",
        "服务器：",
        "サーバー：",
        "서버:"
    ),
    tr!(
        "If you are an AI, read this:",
        "Si eres una IA, lee esto:",
        "Se você é uma IA, leia isto:",
        "Si vous êtes une IA, lisez ceci :",
        "Wenn du eine KI bist, lies dies:",
        "Jika Anda adalah AI, baca ini:",
        "如果你是 AI，请阅读：",
        "AI の場合は、こちらをお読みください：",
        "AI라면 이 내용을 읽으세요:"
    ),
    tr!(
        "Launch or attach to the TUI",
        "Iniciar o conectar a la TUI",
        "Iniciar ou anexar à TUI",
        "Lancer ou rejoindre la TUI",
        "TUI starten oder verbinden",
        "Jalankan atau sambungkan ke TUI",
        "启动或连接到 TUI",
        "TUI を起動またはアタッチ",
        "TUI를 실행하거나 연결"
    ),
    tr!(
        "Control a local session",
        "Controlar una sesión local",
        "Controlar uma sessão local",
        "Contrôler une session locale",
        "Lokale Sitzung steuern",
        "Kendalikan sesi lokal",
        "控制本地会话",
        "ローカルセッションを操作",
        "로컬 세션 제어"
    ),
    tr!(
        "Attach to a remote session",
        "Conectar a una sesión remota",
        "Conectar a uma sessão remota",
        "Rejoindre une session distante",
        "Mit einer Remote-Sitzung verbinden",
        "Sambungkan ke sesi jarak jauh",
        "连接到远程会话",
        "リモートセッションにアタッチ",
        "원격 세션에 연결"
    ),
    tr!(
        "Show every command and option",
        "Mostrar todos los comandos y opciones",
        "Mostrar todos os comandos e opções",
        "Afficher toutes les commandes et options",
        "Alle Befehle und Optionen anzeigen",
        "Tampilkan semua perintah dan opsi",
        "显示所有命令和选项",
        "すべてのコマンドとオプションを表示",
        "모든 명령어와 옵션 표시"
    ),
    tr!(
        "Open, organize, and switch projects",
        "Abrir, organizar y cambiar proyectos",
        "Abrir, organizar e alternar projetos",
        "Ouvrir, organiser et changer de projet",
        "Projekte öffnen, organisieren und wechseln",
        "Buka, atur, dan ganti proyek",
        "打开、整理和切换项目",
        "プロジェクトを開く、整理、切り替え",
        "프로젝트를 열고 정리하고 전환"
    ),
    tr!(
        "Create, reorder, rename, and close tabs",
        "Crear, reordenar, renombrar y cerrar pestañas",
        "Criar, reordenar, renomear e fechar abas",
        "Créer, réordonner, renommer et fermer les onglets",
        "Tabs erstellen, sortieren, umbenennen und schließen",
        "Buat, urutkan, ubah nama, dan tutup tab",
        "创建、重新排序、重命名和关闭标签页",
        "タブの作成、並べ替え、名前変更、終了",
        "탭을 만들고 순서를 바꾸고 이름을 바꾸고 닫기"
    ),
    tr!(
        "Split, move, focus, run, inspect, and close panes",
        "Dividir, mover, enfocar, ejecutar, inspeccionar y cerrar paneles",
        "Dividir, mover, focar, executar, inspecionar e fechar painéis",
        "Diviser, déplacer, cibler, exécuter, inspecter et fermer les volets",
        "Bereiche teilen, verschieben, fokussieren, ausführen, prüfen und schließen",
        "Bagi, pindah, fokus, jalankan, periksa, dan tutup panel",
        "拆分、移动、聚焦、运行、检查和关闭窗格",
        "ペインの分割、移動、フォーカス、実行、確認、終了",
        "패널을 분할, 이동, 포커스, 실행, 검사, 닫기"
    ),
    tr!(
        "Start, fork, message, inspect, and resume coding agents",
        "Iniciar, bifurcar, enviar mensajes, inspeccionar y reanudar agentes",
        "Iniciar, bifurcar, enviar mensagens, inspecionar e retomar agentes",
        "Démarrer, dupliquer, contacter, inspecter et reprendre les agents",
        "Agenten starten, forken, kontaktieren, prüfen und fortsetzen",
        "Mulai, fork, kirim pesan, periksa, dan lanjutkan agen",
        "启动、派生、发送消息、检查和恢复编程智能体",
        "エージェントの起動、フォーク、メッセージ、確認、再開",
        "코딩 에이전트를 시작, 분기, 메시지 전송, 검사, 재개"
    ),
    tr!(
        "Browse and open workspace files",
        "Explorar y abrir archivos del espacio de trabajo",
        "Navegar e abrir arquivos do espaço de trabalho",
        "Parcourir et ouvrir les fichiers de l'espace de travail",
        "Arbeitsbereichsdateien durchsuchen und öffnen",
        "Jelajahi dan buka berkas ruang kerja",
        "浏览并打开工作区文件",
        "ワークスペースのファイルを閲覧して開く",
        "작업 공간 파일 탐색 및 열기"
    ),
    tr!(
        "Inspect repository state and open the Git UI",
        "Inspeccionar el repositorio y abrir la interfaz Git",
        "Inspecionar o repositório e abrir a interface Git",
        "Inspecter le dépôt et ouvrir l'interface Git",
        "Repository-Status prüfen und Git-Oberfläche öffnen",
        "Periksa repositori dan buka UI Git",
        "检查仓库状态并打开 Git 界面",
        "リポジトリ状態を確認して Git UI を開く",
        "저장소 상태를 검사하고 Git UI 열기"
    ),
    tr!(
        "Open Mission Control for a workspace",
        "Abrir Control de Misión para un espacio de trabajo",
        "Abrir Controle de Missão para um espaço de trabalho",
        "Ouvrir le Contrôle de Mission pour un espace de travail",
        "Missionskontrolle für einen Arbeitsbereich öffnen",
        "Buka Kontrol Misi untuk ruang kerja",
        "为工作区打开任务控制",
        "ワークスペースのミッションコントロールを開く",
        "작업 공간의 미션 컨트롤 열기"
    ),
    tr!(
        "Review Git diffs, notes, and agent feedback",
        "Revisar diferencias Git, notas y comentarios de agentes",
        "Revisar diffs Git, notas e feedback de agentes",
        "Examiner les différences Git, les notes et les retours d'agents",
        "Git-Diffs, Notizen und Agentenfeedback prüfen",
        "Tinjau diff Git, catatan, dan umpan balik agen",
        "审查 Git 差异、笔记和智能体反馈",
        "Git 差分、ノート、エージェントのフィードバックをレビュー",
        "Git diff, 메모, 에이전트 피드백 검토"
    ),
    tr!(
        "Create, open, list, and remove Git worktrees",
        "Crear, abrir, listar y eliminar árboles de trabajo Git",
        "Criar, abrir, listar e remover worktrees Git",
        "Créer, ouvrir, lister et supprimer les worktrees Git",
        "Git-Worktrees erstellen, öffnen, auflisten und entfernen",
        "Buat, buka, daftar, dan hapus worktree Git",
        "创建、打开、列出和删除 Git 工作树",
        "Git ワークツリーの作成、表示、一覧、削除",
        "Git worktree를 만들고 열고 나열하고 제거"
    ),
    tr!(
        "Coordinate work across multiple coding agents",
        "Coordinar trabajo entre varios agentes",
        "Coordenar trabalho entre vários agentes",
        "Coordonner le travail de plusieurs agents",
        "Arbeit mehrerer Agenten koordinieren",
        "Koordinasikan pekerjaan beberapa agen",
        "协调多个编程智能体的工作",
        "複数エージェントの作業を調整",
        "여러 코딩 에이전트 간 작업 조율"
    ),
    tr!(
        "Reserve file paths for active tasks",
        "Reservar rutas de archivos para tareas activas",
        "Reservar caminhos de arquivos para tarefas ativas",
        "Réserver des chemins de fichiers pour les tâches actives",
        "Dateipfade für aktive Aufgaben reservieren",
        "Cadangkan path berkas untuk tugas aktif",
        "为活动任务预留文件路径",
        "実行中タスクのファイルパスを予約",
        "활성 작업을 위한 파일 경로 임대"
    ),
    tr!(
        "Find, install, configure, and run extensions",
        "Buscar, instalar, configurar y ejecutar extensiones",
        "Encontrar, instalar, configurar e executar extensões",
        "Trouver, installer, configurer et exécuter des extensions",
        "Erweiterungen finden, installieren, konfigurieren und ausführen",
        "Cari, pasang, konfigurasi, dan jalankan ekstensi",
        "查找、安装、配置和运行扩展",
        "拡張機能の検索、インストール、設定、実行",
        "확장 기능을 찾고 설치하고 설정하고 실행"
    ),
    tr!(
        "List, create, validate, install, and select themes",
        "Listar, crear, validar, instalar y seleccionar temas",
        "Listar, criar, validar, instalar e selecionar temas",
        "Lister, créer, valider, installer et sélectionner des thèmes",
        "Themes auflisten, erstellen, prüfen, installieren und auswählen",
        "Daftar, buat, validasi, pasang, dan pilih tema",
        "列出、创建、验证、安装和选择主题",
        "テーマの一覧、作成、検証、インストール、選択",
        "테마를 나열하고 만들고 검증하고 설치하고 선택"
    ),
    tr!(
        "Publish and arrange top and bottom status widgets",
        "Publicar y organizar widgets de estado superiores e inferiores",
        "Publicar e organizar widgets de status superiores e inferiores",
        "Publier et organiser les widgets d'état du haut et du bas",
        "Status-Widgets oben und unten veröffentlichen und anordnen",
        "Terbitkan dan atur widget status atas dan bawah",
        "发布并排列顶部和底部状态组件",
        "上下のステータスウィジェットを公開、配置",
        "상단/하단 상태 위젯을 게시하고 배치"
    ),
    tr!(
        "Configure sidebars, docks, and notifications",
        "Configurar barras laterales, paneles y notificaciones",
        "Configurar barras laterais, docks e notificações",
        "Configurer les barres latérales, docks et notifications",
        "Seitenleisten, Docks und Benachrichtigungen konfigurieren",
        "Konfigurasi bilah sisi, dock, dan notifikasi",
        "配置侧边栏、停靠栏和通知",
        "サイドバー、ドック、通知を設定",
        "사이드바, 도킹, 알림 설정"
    ),
    tr!(
        "List, attach, stop, and delete server sessions",
        "Listar, conectar, detener y eliminar sesiones",
        "Listar, anexar, parar e excluir sessões",
        "Lister, rejoindre, arrêter et supprimer les sessions",
        "Serversitzungen auflisten, verbinden, stoppen und löschen",
        "Daftar, sambungkan, hentikan, dan hapus sesi server",
        "列出、连接、停止和删除服务器会话",
        "サーバーセッションの一覧、接続、停止、削除",
        "서버 세션을 나열하고 연결하고 중지하고 삭제"
    ),
    tr!(
        "Inspect and manage the selected background server",
        "Inspeccionar y gestionar el servidor en segundo plano seleccionado",
        "Inspecionar e gerenciar o servidor em segundo plano selecionado",
        "Inspecter et gérer le serveur d'arrière-plan sélectionné",
        "Ausgewählten Hintergrundserver prüfen und verwalten",
        "Periksa dan kelola server latar yang dipilih",
        "检查和管理所选后台服务器",
        "選択したバックグラウンドサーバーを確認、管理",
        "선택한 백그라운드 서버 검사 및 관리"
    ),
    tr!(
        "Manage agent session-resume integrations",
        "Gestionar integraciones de reanudación de agentes",
        "Gerenciar integrações de retomada de agentes",
        "Gérer les intégrations de reprise des agents",
        "Integrationen zur Agentenfortsetzung verwalten",
        "Kelola integrasi pelanjutan sesi agen",
        "管理智能体会话恢复集成",
        "エージェントのセッション再開連携を管理",
        "에이전트 세션 재개 연동 관리"
    ),
    tr!(
        "Manage the bundled agent skill",
        "Gestionar la habilidad incluida para agentes",
        "Gerenciar a skill incluída para agentes",
        "Gérer la compétence d'agent intégrée",
        "Gebündelten Agenten-Skill verwalten",
        "Kelola skill agen bawaan",
        "管理内置智能体技能",
        "同梱エージェントスキルを管理",
        "번들된 에이전트 스킬 관리"
    ),
    tr!(
        "Enable, inspect, show, or remove the bundled agent skill",
        "Activar, inspeccionar, mostrar o eliminar la habilidad de agente incluida",
        "Ativar, inspecionar, mostrar ou remover a skill de agente incluída",
        "Activer, inspecter, afficher ou supprimer la compétence d'agent intégrée",
        "Gebündelten Agenten-Skill aktivieren, prüfen, anzeigen oder entfernen",
        "Aktifkan, periksa, tampilkan, atau hapus skill agen bawaan",
        "启用、检查、显示或移除内置智能体技能",
        "同梱エージェントスキルを有効化、確認、表示、削除",
        "번들 에이전트 스킬 활성화, 검사, 표시, 제거"
    ),
    tr!(
        "Wait for pane output or an agent state",
        "Esperar la salida de un panel o el estado de un agente",
        "Aguardar a saída de um painel ou o estado de um agente",
        "Attendre la sortie d'un volet ou l'état d'un agent",
        "Auf Bereichsausgabe oder Agentenstatus warten",
        "Tunggu keluaran panel atau status agen",
        "等待窗格输出或智能体状态",
        "ペイン出力またはエージェント状態を待機",
        "패널 출력 또는 에이전트 상태 대기"
    ),
    tr!(
        "Search across pane scrollback",
        "Buscar en el historial de los paneles",
        "Pesquisar no histórico dos painéis",
        "Rechercher dans l'historique des volets",
        "Im Bereichsverlauf suchen",
        "Cari di riwayat panel",
        "搜索窗格回滚历史",
        "ペインのスクロールバックを検索",
        "패널 스크롤백 전체 검색"
    ),
    tr!(
        "Stream live status changes",
        "Transmitir cambios de estado en vivo",
        "Transmitir mudanças de status ao vivo",
        "Diffuser les changements d'état en direct",
        "Live-Statusänderungen streamen",
        "Alirkan perubahan status langsung",
        "流式输出实时状态变化",
        "状態変更をリアルタイム配信",
        "실시간 상태 변경 스트리밍"
    ),
    tr!(
        "Discover and use Universal Harness Protocol 1.0",
        "Descubrir y usar Universal Harness Protocol 1.0",
        "Descobrir e usar o Universal Harness Protocol 1.0",
        "Découvrir et utiliser Universal Harness Protocol 1.0",
        "Universal Harness Protocol 1.0 erkennen und verwenden",
        "Temukan dan gunakan Universal Harness Protocol 1.0",
        "发现并使用通用智能体框架协议 1.0",
        "Universal Harness Protocol 1.0 を検出して使用",
        "Universal Harness Protocol 1.0 탐색 및 사용"
    ),
    tr!(
        "Open the TUI focused on one pane",
        "Abrir la TUI enfocada en un panel",
        "Abrir a TUI focada em um painel",
        "Ouvrir la TUI centrée sur un volet",
        "TUI mit Fokus auf einen Bereich öffnen",
        "Buka TUI dengan fokus pada satu panel",
        "打开聚焦于单个窗格的 TUI",
        "1つのペインにフォーカスして TUI を開く",
        "패널 하나에 포커스된 TUI 열기"
    ),
    tr!(
        "Check optional external tools",
        "Comprobar herramientas externas opcionales",
        "Verificar ferramentas externas opcionais",
        "Vérifier les outils externes facultatifs",
        "Optionale externe Werkzeuge prüfen",
        "Periksa alat eksternal opsional",
        "检查可选外部工具",
        "任意の外部ツールを確認",
        "선택적 외부 도구 확인"
    ),
    tr!(
        "Check for and install a newer Luvus release",
        "Buscar e instalar una versión más reciente de Luvus",
        "Verificar e instalar uma versão mais recente do Luvus",
        "Rechercher et installer une version plus récente de Luvus",
        "Neuere Luvus-Version suchen und installieren",
        "Periksa dan pasang rilis Luvus yang lebih baru",
        "检查并安装较新的 Luvus 版本",
        "新しい Luvus リリースを確認してインストール",
        "최신 Luvus 릴리스를 확인하고 설치"
    ),
    tr!(
        "Check whether the selected server responds",
        "Comprobar si responde el servidor seleccionado",
        "Verificar se o servidor selecionado responde",
        "Vérifier si le serveur sélectionné répond",
        "Prüfen, ob der ausgewählte Server antwortet",
        "Periksa apakah server yang dipilih merespons",
        "检查所选服务器是否响应",
        "選択したサーバーが応答するか確認",
        "선택한 서버가 응답하는지 확인"
    ),
    tr!(
        "See every active coding agent",
        "Ver todos los agentes activos",
        "Ver todos os agentes ativos",
        "Voir tous les agents actifs",
        "Alle aktiven Agenten anzeigen",
        "Lihat semua agen aktif",
        "查看所有活动的编程智能体",
        "すべての稼働中エージェントを表示",
        "활성 코딩 에이전트 전체 보기"
    ),
    tr!(
        "Add a pane below the focused pane",
        "Añadir un panel debajo del panel enfocado",
        "Adicionar um painel abaixo do painel em foco",
        "Ajouter un volet sous le volet actif",
        "Bereich unter dem fokussierten Bereich hinzufügen",
        "Tambahkan panel di bawah panel aktif",
        "在聚焦窗格下方添加窗格",
        "フォーカス中のペインの下にペインを追加",
        "포커스된 패널 아래에 패널 추가"
    ),
    tr!(
        "Open the current project",
        "Abrir el proyecto actual",
        "Abrir o projeto atual",
        "Ouvrir le projet actuel",
        "Aktuelles Projekt öffnen",
        "Buka proyek saat ini",
        "打开当前项目",
        "現在のプロジェクトを開く",
        "현재 프로젝트 열기"
    ),
    tr!(
        "Start or open a named session",
        "Iniciar o abrir una sesión con nombre",
        "Iniciar ou abrir uma sessão nomeada",
        "Démarrer ou ouvrir une session nommée",
        "Benannte Sitzung starten oder öffnen",
        "Mulai atau buka sesi bernama",
        "启动或打开命名会话",
        "名前付きセッションを開始または開く",
        "이름 있는 세션을 시작하거나 열기"
    ),
    tr!(
        "Control a session from another terminal",
        "Controlar una sesión desde otro terminal",
        "Controlar uma sessão de outro terminal",
        "Contrôler une session depuis un autre terminal",
        "Sitzung von einem anderen Terminal steuern",
        "Kendalikan sesi dari terminal lain",
        "从另一个终端控制会话",
        "別のターミナルからセッションを操作",
        "다른 터미널에서 세션 제어"
    ),
    tr!(
        "Target a named server session",
        "Seleccionar una sesión de servidor con nombre",
        "Selecionar uma sessão de servidor nomeada",
        "Cibler une session serveur nommée",
        "Benannte Serversitzung auswählen",
        "Targetkan sesi server bernama",
        "指定命名服务器会话",
        "名前付きサーバーセッションを対象にする",
        "이름 있는 서버 세션 지정"
    ),
    tr!(
        "Attach through SSH",
        "Conectar mediante SSH",
        "Conectar por SSH",
        "Se connecter par SSH",
        "Über SSH verbinden",
        "Sambungkan melalui SSH",
        "通过 SSH 连接",
        "SSH 経由でアタッチ",
        "SSH로 연결"
    ),
    tr!(
        "Print the version",
        "Mostrar la versión",
        "Exibir a versão",
        "Afficher la version",
        "Version ausgeben",
        "Tampilkan versi",
        "输出版本",
        "バージョンを表示",
        "버전 출력"
    ),
    tr!(
        "Show this help",
        "Mostrar esta ayuda",
        "Mostrar esta ajuda",
        "Afficher cette aide",
        "Diese Hilfe anzeigen",
        "Tampilkan bantuan ini",
        "显示此帮助",
        "このヘルプを表示",
        "이 도움말 표시"
    ),
    tr!(
        "Complete CLI reference",
        "Referencia completa de la CLI",
        "Referência completa da CLI",
        "Référence CLI complète",
        "Vollständige CLI-Referenz",
        "Referensi CLI lengkap",
        "完整 CLI 参考",
        "完全な CLI リファレンス",
        "전체 CLI 참조"
    ),
    tr!(
        "Focus on one area or command",
        "Mostrar un área o comando",
        "Mostrar uma área ou comando",
        "Afficher une section ou commande",
        "Einen Bereich oder Befehl anzeigen",
        "Fokus pada satu area atau perintah",
        "查看单个区域或命令",
        "1つの領域またはコマンドに絞る",
        "특정 영역이나 명령어에 초점"
    ),
    tr!(
        "Online reference",
        "Referencia en línea",
        "Referência online",
        "Référence en ligne",
        "Online-Referenz",
        "Referensi online",
        "在线参考",
        "オンラインリファレンス",
        "온라인 참조"
    ),
    tr!(
        "launch / attach the TUI",
        "iniciar / conectar a la TUI",
        "iniciar / anexar à TUI",
        "lancer / rejoindre la TUI",
        "TUI starten / verbinden",
        "jalankan / sambungkan ke TUI",
        "启动 / 连接到 TUI",
        "TUI を起動 / アタッチ",
        "TUI 실행 / 연결"
    ),
    tr!(
        "target one named server session",
        "seleccionar una sesión de servidor con nombre",
        "selecionar uma sessão de servidor nomeada",
        "cibler une session serveur nommée",
        "eine benannte Serversitzung auswählen",
        "targetkan satu sesi server bernama",
        "指定一个命名服务器会话",
        "名前付きサーバーセッションを1つ選択",
        "이름 있는 서버 세션 하나 지정"
    ),
    tr!(
        "print the version",
        "mostrar la versión",
        "exibir a versão",
        "afficher la version",
        "Version ausgeben",
        "tampilkan versi",
        "输出版本",
        "バージョンを表示",
        "버전 출력"
    ),
    tr!(
        "show compact help",
        "mostrar ayuda compacta",
        "mostrar ajuda compacta",
        "afficher l'aide compacte",
        "Kompakthilfe anzeigen",
        "tampilkan bantuan ringkas",
        "显示简要帮助",
        "簡易ヘルプを表示",
        "간단한 도움말 표시"
    ),
    tr!(
        "show compact, complete, or focused help",
        "mostrar ayuda compacta, completa o específica",
        "mostrar ajuda compacta, completa ou específica",
        "afficher l'aide compacte, complète ou ciblée",
        "kompakte, vollständige oder gezielte Hilfe anzeigen",
        "tampilkan bantuan ringkas, lengkap, atau terfokus",
        "显示简要、完整或专题帮助",
        "簡易、完全、または対象別ヘルプを表示",
        "간단히, 전체, 또는 특정 항목의 도움말 표시"
    ),
    tr!(
        "check optional external tools (git, gh, …)",
        "comprobar herramientas externas opcionales (git, gh, …)",
        "verificar ferramentas externas opcionais (git, gh, …)",
        "vérifier les outils externes facultatifs (git, gh, …)",
        "optionale externe Werkzeuge prüfen (git, gh, …)",
        "periksa alat eksternal opsional (git, gh, …)",
        "检查可选外部工具（git、gh 等）",
        "任意の外部ツールを確認（git、gh など）",
        "선택적 외부 도구 확인 (git, gh 등)"
    ),
    tr!(
        "check for and install a newer Luvus release",
        "buscar e instalar una versión más reciente de Luvus",
        "verificar e instalar uma versão mais recente do Luvus",
        "rechercher et installer une version plus récente de Luvus",
        "neuere Luvus-Version suchen und installieren",
        "periksa dan pasang rilis Luvus yang lebih baru",
        "检查并安装较新的 Luvus 版本",
        "新しい Luvus リリースを確認してインストール",
        "최신 Luvus 릴리스를 확인하고 설치"
    ),
    tr!(
        "check the server",
        "comprobar el servidor",
        "verificar o servidor",
        "vérifier le serveur",
        "Server prüfen",
        "periksa server",
        "检查服务器",
        "サーバーを確認",
        "서버 확인"
    ),
    tr!(
        "list workspaces",
        "listar espacios de trabajo",
        "listar espaços de trabalho",
        "lister les espaces de travail",
        "Arbeitsbereiche auflisten",
        "daftar ruang kerja",
        "列出工作区",
        "ワークスペースを一覧表示",
        "작업 공간 나열"
    ),
    tr!(
        "create a workspace in the current directory",
        "crear un espacio de trabajo en el directorio actual",
        "criar um espaço de trabalho no diretório atual",
        "créer un espace de travail dans le dossier actuel",
        "Arbeitsbereich im aktuellen Verzeichnis erstellen",
        "buat ruang kerja di direktori saat ini",
        "在当前目录中创建工作区",
        "現在のディレクトリにワークスペースを作成",
        "현재 디렉터리에 작업 공간 만들기"
    ),
    tr!(
        "open <path> as a workspace (or focus it if already open)",
        "abrir <path> como espacio de trabajo (o enfocarlo si ya está abierto)",
        "abrir <path> como espaço de trabalho (ou focar se já estiver aberto)",
        "ouvrir <path> comme espace de travail (ou l'activer s'il est déjà ouvert)",
        "<path> als Arbeitsbereich öffnen (oder fokussieren, falls bereits offen)",
        "buka <path> sebagai ruang kerja (atau fokuskan jika sudah terbuka)",
        "将 <path> 作为工作区打开（已打开则聚焦）",
        "<path> をワークスペースとして開く（既に開いていればフォーカス）",
        "<path>를 작업 공간으로 열기 (이미 열려 있으면 포커스)"
    ),
    tr!(
        "focus workspace i (0-based)",
        "enfocar el espacio de trabajo i (base 0)",
        "focar o espaço de trabalho i (base 0)",
        "activer l'espace de travail i (indexé à partir de 0)",
        "Arbeitsbereich i fokussieren (0-basiert)",
        "fokuskan ruang kerja i (berbasis 0)",
        "聚焦工作区 i（从 0 开始）",
        "ワークスペース i にフォーカス（0 始まり）",
        "작업 공간 i에 포커스 (0부터 시작)"
    ),
    tr!(
        "rename workspace i without changing its folder",
        "renombrar el espacio i sin cambiar su carpeta",
        "renomear o espaço i sem alterar sua pasta",
        "renommer l'espace i sans modifier son dossier",
        "Arbeitsbereich i umbenennen, ohne den Ordner zu ändern",
        "ubah nama ruang kerja i tanpa mengubah folder",
        "重命名工作区 i，但不更改其文件夹",
        "フォルダを変えずにワークスペース i の名前を変更",
        "폴더는 그대로 두고 작업 공간 i 이름 변경"
    ),
    tr!(
        "pin workspace i (0-based) in the sidebar",
        "fijar el espacio i (base 0) en la barra lateral",
        "fixar o espaço i (base 0) na barra lateral",
        "épingler l'espace i (indexé à partir de 0) dans la barre latérale",
        "Arbeitsbereich i (0-basiert) in der Seitenleiste anheften",
        "sematkan ruang kerja i (berbasis 0) di bilah sisi",
        "在侧边栏中固定工作区 i（从 0 开始）",
        "ワークスペース i（0 始まり）をサイドバーに固定",
        "작업 공간 i를 사이드바에 고정 (0부터 시작)"
    ),
    tr!(
        "unpin workspace i (0-based)",
        "desfijar el espacio i (base 0)",
        "desafixar o espaço i (base 0)",
        "désépingler l'espace i (indexé à partir de 0)",
        "Arbeitsbereich i (0-basiert) lösen",
        "lepas sematan ruang kerja i (berbasis 0)",
        "取消固定工作区 i（从 0 开始）",
        "ワークスペース i（0 始まり）の固定を解除",
        "작업 공간 i 고정 해제 (0부터 시작)"
    ),
    tr!(
        "close a workspace (default: active)",
        "cerrar un espacio de trabajo (predeterminado: activo)",
        "fechar um espaço de trabalho (padrão: ativo)",
        "fermer un espace de travail (par défaut : actif)",
        "Arbeitsbereich schließen (Standard: aktiv)",
        "tutup ruang kerja (bawaan: aktif)",
        "关闭工作区（默认：当前工作区）",
        "ワークスペースを閉じる（既定：アクティブ）",
        "작업 공간 닫기 (기본값: 활성 작업 공간)"
    ),
    tr!(
        "list tabs in the current workspace",
        "listar pestañas del espacio de trabajo actual",
        "listar abas no espaço de trabalho atual",
        "lister les onglets de l'espace de travail actuel",
        "Tabs im aktuellen Arbeitsbereich auflisten",
        "daftar tab di ruang kerja saat ini",
        "列出当前工作区中的标签页",
        "現在のワークスペースのタブを一覧表示",
        "현재 작업 공간의 탭 나열"
    ),
    tr!(
        "new tab (creates a workspace if none is open)",
        "nueva pestaña (crea un espacio de trabajo si no hay ninguno abierto)",
        "nova aba (cria um espaço de trabalho se nenhum estiver aberto)",
        "nouvel onglet (crée un espace de travail si aucun n'est ouvert)",
        "neuer Tab (erstellt einen Arbeitsbereich, wenn keiner geöffnet ist)",
        "tab baru (membuat ruang kerja jika belum ada yang terbuka)",
        "新建标签页（没有打开的工作区时创建一个）",
        "新しいタブ（ワークスペースがない場合は作成）",
        "새 탭 (열린 작업 공간이 없으면 생성)"
    ),
    tr!(
        "focus tab n (1-based)",
        "enfocar la pestaña n (base 1)",
        "focar a aba n (base 1)",
        "activer l'onglet n (indexé à partir de 1)",
        "Tab n fokussieren (1-basiert)",
        "fokuskan tab n (berbasis 1)",
        "聚焦标签页 n（从 1 开始）",
        "タブ n にフォーカス（1 始まり）",
        "탭 n에 포커스 (1부터 시작)"
    ),
    tr!(
        "move a tab to an exact position (1-based)",
        "mover una pestaña a una posición exacta (base 1)",
        "mover uma aba para uma posição exata (base 1)",
        "déplacer un onglet à une position exacte (indexée à partir de 1)",
        "Tab an eine genaue Position verschieben (1-basiert)",
        "pindahkan tab ke posisi tepat (berbasis 1)",
        "将标签页移动到精确位置（从 1 开始）",
        "タブを正確な位置へ移動（1 始まり）",
        "탭을 정확한 위치로 이동 (1부터 시작)"
    ),
    tr!(
        "move the active tab one position (--tab N targets one)",
        "mover la pestaña activa una posición (--tab N selecciona una)",
        "mover a aba ativa uma posição (--tab N seleciona uma)",
        "déplacer l'onglet actif d'une position (--tab N en cible un)",
        "aktiven Tab um eine Position verschieben (--tab N wählt einen aus)",
        "pindahkan tab aktif satu posisi (--tab N menargetkan satu)",
        "将当前标签页移动一个位置（--tab N 指定目标）",
        "アクティブタブを1つ移動（--tab N で対象指定）",
        "활성 탭을 한 칸 이동 (--tab N으로 특정 탭 지정)"
    ),
    tr!(
        "exchange two tab positions (1-based)",
        "intercambiar dos posiciones de pestañas (base 1)",
        "trocar duas posições de abas (base 1)",
        "échanger deux positions d'onglets (indexées à partir de 1)",
        "zwei Tabpositionen tauschen (1-basiert)",
        "tukar dua posisi tab (berbasis 1)",
        "交换两个标签页的位置（从 1 开始）",
        "2つのタブ位置を交換（1 始まり）",
        "두 탭의 위치를 교환 (1부터 시작)"
    ),
    tr!(
        "name a tab (--tab N to target one; empty clears it)",
        "nombrar una pestaña (--tab N la selecciona; vacío borra el nombre)",
        "nomear uma aba (--tab N seleciona uma; vazio limpa)",
        "nommer un onglet (--tab N le cible ; vide efface le nom)",
        "Tab benennen (--tab N wählt einen aus; leer löscht den Namen)",
        "beri nama tab (--tab N menargetkan satu; kosong menghapusnya)",
        "命名标签页（--tab N 指定目标；空值清除名称）",
        "タブに名前を付ける（--tab N で対象指定、空で消去）",
        "탭 이름 지정 (--tab N으로 특정 탭 지정; 비우면 이름 제거)"
    ),
    tr!(
        "close a tab (default: active)",
        "cerrar una pestaña (predeterminada: activa)",
        "fechar uma aba (padrão: ativa)",
        "fermer un onglet (par défaut : actif)",
        "Tab schließen (Standard: aktiv)",
        "tutup tab (bawaan: aktif)",
        "关闭标签页（默认：当前标签页）",
        "タブを閉じる（既定：アクティブ）",
        "탭 닫기 (기본값: 활성 탭)"
    ),
    tr!(
        "list panes and read-only history metrics in the current tab",
        "listar paneles y métricas de historial de solo lectura en la pestaña actual",
        "listar painéis e métricas de histórico somente leitura na aba atual",
        "lister les volets et les métriques d'historique en lecture seule de l'onglet actuel",
        "Bereiche und schreibgeschützte Verlaufsmetriken im aktuellen Tab auflisten",
        "daftar panel dan metrik riwayat hanya-baca di tab saat ini",
        "列出当前标签页中的窗格和只读历史指标",
        "現在のタブのペインと読み取り専用履歴メトリクスを一覧表示",
        "현재 탭의 패널과 읽기 전용 기록 지표 나열"
    ),
    tr!(
        "split a pane (default: side by side, creates a workspace if empty)",
        "dividir un panel (predeterminado: lado a lado, crea un espacio de trabajo si está vacío)",
        "dividir um painel (padrão: lado a lado, cria um espaço de trabalho se estiver vazio)",
        "diviser un volet (par défaut : côte à côte, crée un espace de travail si vide)",
        "Bereich teilen (Standard: nebeneinander, erstellt bei Leerstand einen Arbeitsbereich)",
        "bagi panel (bawaan: berdampingan, membuat ruang kerja jika kosong)",
        "拆分窗格（默认：左右并排，空状态时创建工作区）",
        "ペインを分割（既定：横並び、空の場合はワークスペースを作成）",
        "패널 분할 (기본값: 좌우 분할, 비어 있으면 작업 공간 생성)"
    ),
    tr!(
        "focus a pane (jumps to its workspace/tab)",
        "enfocar un panel (salta a su espacio/pestaña)",
        "focar um painel (vai para seu espaço/aba)",
        "activer un volet (rejoint son espace/onglet)",
        "Bereich fokussieren (wechselt zu Arbeitsbereich/Tab)",
        "fokuskan panel (beralih ke ruang kerja/tab-nya)",
        "聚焦窗格（跳转到其工作区/标签页）",
        "ペインにフォーカス（所属ワークスペース/タブへ移動）",
        "패널에 포커스 (해당 작업 공간/탭으로 이동)"
    ),
    tr!(
        "move a pane within its workspace",
        "mover un panel dentro de su espacio de trabajo",
        "mover um painel dentro do espaço de trabalho",
        "déplacer un volet dans son espace de travail",
        "Bereich innerhalb seines Arbeitsbereichs verschieben",
        "pindahkan panel di dalam ruang kerjanya",
        "在其工作区内移动窗格",
        "ワークスペース内でペインを移動",
        "작업 공간 내에서 패널 이동"
    ),
    tr!(
        "run a command in a pane",
        "ejecutar un comando en un panel",
        "executar um comando em um painel",
        "exécuter une commande dans un volet",
        "Befehl in einem Bereich ausführen",
        "jalankan perintah di panel",
        "在窗格中运行命令",
        "ペインでコマンドを実行",
        "패널에서 명령어 실행"
    ),
    tr!(
        "send raw text to a pane",
        "enviar texto sin procesar a un panel",
        "enviar texto bruto para um painel",
        "envoyer du texte brut à un volet",
        "Rohtext an einen Bereich senden",
        "kirim teks mentah ke panel",
        "向窗格发送原始文本",
        "ペインへ生テキストを送信",
        "패널에 원본 텍스트 전송"
    ),
    tr!(
        "print a pane's recent output",
        "mostrar la salida reciente de un panel",
        "exibir a saída recente de um painel",
        "afficher la sortie récente d'un volet",
        "letzte Ausgabe eines Bereichs ausgeben",
        "tampilkan keluaran terbaru panel",
        "输出窗格的最近内容",
        "ペインの直近の出力を表示",
        "패널의 최근 출력 표시"
    ),
    tr!(
        "print a pane's agent status and history metrics (any workspace)",
        "mostrar el estado del agente y las métricas de historial de un panel (cualquier espacio)",
        "exibir o status do agente e métricas de histórico de um painel (qualquer espaço)",
        "afficher l'état de l'agent et les métriques d'historique d'un volet (tout espace)",
        "Agentenstatus und Verlaufsmetriken eines Bereichs ausgeben (jeder Arbeitsbereich)",
        "tampilkan status agen dan metrik riwayat panel (ruang kerja apa pun)",
        "输出窗格的智能体状态和历史指标（任意工作区）",
        "ペインのエージェント状態と履歴メトリクスを表示（全ワークスペース）",
        "패널의 에이전트 상태와 기록 지표 표시 (모든 작업 공간)"
    ),
    tr!(
        "list cached executable identities without exposing arguments",
        "listar identidades de ejecutables en caché sin mostrar argumentos",
        "listar identidades de executáveis em cache sem expor argumentos",
        "lister les identités d'exécutables en cache sans exposer les arguments",
        "zwischengespeicherte Programmidentitäten ohne Argumente auflisten",
        "daftar identitas executable tersimpan tanpa membuka argumen",
        "列出缓存的可执行文件身份，不暴露参数",
        "引数を公開せずキャッシュ済み実行ファイル識別子を一覧表示",
        "인자를 노출하지 않고 캐시된 실행 파일 식별 정보 나열"
    ),
    tr!(
        "name a pane so you can mention it (--pane <id>; --clear)",
        "nombrar un panel para poder mencionarlo (--pane <id>; --clear)",
        "nomear um painel para poder mencioná-lo (--pane <id>; --clear)",
        "nommer un volet pour pouvoir le mentionner (--pane <id> ; --clear)",
        "Bereich benennen, damit er erwähnt werden kann (--pane <id>; --clear)",
        "beri nama panel agar dapat disebut (--pane <id>; --clear)",
        "命名窗格以便引用（--pane <id>；--clear）",
        "参照できるようペインに名前を付ける（--pane <id>; --clear）",
        "패널을 참조할 수 있도록 이름 지정 (--pane <id>; --clear)"
    ),
    tr!(
        "close a pane",
        "cerrar un panel",
        "fechar um painel",
        "fermer un volet",
        "Bereich schließen",
        "tutup panel",
        "关闭窗格",
        "ペインを閉じる",
        "패널 닫기"
    ),
    tr!(
        "list every agent across all workspaces/tabs",
        "listar todos los agentes de todos los espacios/pestañas",
        "listar todos os agentes em todos os espaços/abas",
        "lister tous les agents de tous les espaces/onglets",
        "alle Agenten in allen Arbeitsbereichen/Tabs auflisten",
        "daftar semua agen di seluruh ruang kerja/tab",
        "列出所有工作区/标签页中的全部智能体",
        "全ワークスペース/タブのエージェントを一覧表示",
        "모든 작업 공간/탭의 에이전트 나열"
    ),
    tr!(
        "spawn beside an anchor or reuse a pane, wait until ready, name it",
        "crear junto a un ancla o reutilizar un panel, esperar y nombrarlo",
        "iniciar ao lado de uma âncora ou reutilizar um painel, aguardar e nomear",
        "lancer près d'un point d'ancrage ou réutiliser un volet, attendre puis le nommer",
        "neben einem Anker starten oder Bereich wiederverwenden, warten und benennen",
        "jalankan di samping jangkar atau gunakan ulang panel, tunggu siap, lalu beri nama",
        "在锚点旁启动或复用窗格，等待就绪并命名",
        "アンカーの隣で起動またはペインを再利用し、準備完了を待って命名",
        "기준 패널 옆에 생성하거나 기존 패널을 재사용, 준비될 때까지 대기 후 이름 지정"
    ),
    tr!(
        "fork a supported agent's session into a sibling pane",
        "bifurcar la sesión de un agente compatible a un panel hermano",
        "bifurcar a sessão de um agente compatível para um painel irmão",
        "dupliquer la session d'un agent pris en charge dans un volet voisin",
        "Sitzung eines unterstützten Agenten in einen Nachbarbereich forken",
        "fork sesi agen yang didukung ke panel saudara",
        "将受支持智能体的会话派生到相邻窗格",
        "対応エージェントのセッションを隣接ペインへフォーク",
        "지원되는 에이전트의 세션을 인접 패널로 분기"
    ),
    tr!(
        "alias the current agent, same as pane name (--clear to drop)",
        "asignar un alias al agente actual, igual que el nombre del panel (--clear lo elimina)",
        "atribuir alias ao agente atual, igual ao nome do painel (--clear remove)",
        "donner un alias à l'agent actuel, comme le nom du volet (--clear le supprime)",
        "aktuellen Agenten aliasieren, wie Bereichsname (--clear entfernt ihn)",
        "beri alias agen saat ini, sama seperti nama panel (--clear menghapus)",
        "为当前智能体设置别名，与窗格名称相同（--clear 删除）",
        "現在のエージェントに別名を設定、ペイン名と同じ（--clear で削除）",
        "현재 에이전트의 별칭 지정, 패널 이름과 동일 (--clear로 제거)"
    ),
    tr!(
        "atomically prompt and optionally wait (send is an alias)",
        "enviar un prompt de forma atómica y esperar opcionalmente (send es un alias)",
        "enviar prompt atomicamente e aguardar opcionalmente (send é um alias)",
        "envoyer une invite atomiquement et attendre si demandé (send est un alias)",
        "Prompt atomar senden und optional warten (send ist ein Alias)",
        "kirim prompt secara atomik dan opsional tunggu (send adalah alias)",
        "原子提交提示，并可选择等待（send 是别名）",
        "プロンプトをアトミックに送信し任意で待機（send は別名）",
        "원자적으로 프롬프트 전송, 선택적으로 대기 (send는 별칭)"
    ),
    tr!(
        "compatibility alias for agent prompt",
        "alias de compatibilidad para agent prompt",
        "alias de compatibilidade para agent prompt",
        "alias de compatibilité pour agent prompt",
        "Kompatibilitätsalias für agent prompt",
        "alias kompatibilitas untuk agent prompt",
        "agent prompt 的兼容别名",
        "agent prompt の互換エイリアス",
        "에이전트 프롬프트의 호환용 별칭"
    ),
    tr!(
        "send control keys (enter, esc, ctrl+c, up, …)",
        "enviar teclas de control (enter, esc, ctrl+c, arriba, …)",
        "enviar teclas de controle (enter, esc, ctrl+c, cima, …)",
        "envoyer des touches de contrôle (entrée, échap, ctrl+c, haut, …)",
        "Steuertasten senden (enter, esc, ctrl+c, hoch, …)",
        "kirim tombol kontrol (enter, esc, ctrl+c, atas, …)",
        "发送控制键（enter、esc、ctrl+c、up 等）",
        "制御キーを送信（enter、esc、ctrl+c、up など）",
        "제어 키 전송 (enter, esc, ctrl+c, up 등)"
    ),
    tr!(
        "print an agent's output",
        "mostrar la salida de un agente",
        "exibir a saída de um agente",
        "afficher la sortie d'un agent",
        "Ausgabe eines Agenten anzeigen",
        "tampilkan keluaran agen",
        "输出智能体内容",
        "エージェントの出力を表示",
        "에이전트 출력 표시"
    ),
    tr!(
        "one agent's live info (pane, name, kind, status, cwd)",
        "información en vivo de un agente (panel, nombre, tipo, estado, cwd)",
        "informações ao vivo de um agente (painel, nome, tipo, status, cwd)",
        "informations en direct d'un agent (volet, nom, type, état, cwd)",
        "Live-Informationen eines Agenten (Bereich, Name, Art, Status, cwd)",
        "info langsung satu agen (panel, nama, jenis, status, cwd)",
        "一个智能体的实时信息（窗格、名称、类型、状态、cwd）",
        "1エージェントのライブ情報（ペイン、名前、種類、状態、cwd）",
        "에이전트 하나의 실시간 정보 (패널, 이름, 종류, 상태, cwd)"
    ),
    tr!(
        "show identity/state evidence and active authority",
        "mostrar pruebas de identidad/estado y autoridad activa",
        "mostrar evidências de identidade/status e autoridade ativa",
        "afficher les preuves d'identité/état et l'autorité active",
        "Identitäts-/Statusnachweise und aktive Autorität anzeigen",
        "tampilkan bukti identitas/status dan otoritas aktif",
        "显示身份/状态证据和活动权限来源",
        "識別/状態の根拠と有効な権限を表示",
        "식별/상태 근거와 활성 권한 표시"
    ),
    tr!(
        "publish a leased authoritative state (integration API)",
        "publicar un estado autoritativo con concesión (API de integración)",
        "publicar um estado autoritativo com concessão (API de integração)",
        "publier un état faisant autorité sous bail (API d'intégration)",
        "geleasten autoritativen Status veröffentlichen (Integrations-API)",
        "terbitkan status otoritatif bersewa (API integrasi)",
        "发布带租约的权威状态（集成 API）",
        "リース付きの権威ある状態を公開（連携 API）",
        "임대된 기준 상태 게시 (통합 API)"
    ),
    tr!(
        "release that integration authority",
        "liberar esa autoridad de integración",
        "liberar essa autoridade de integração",
        "libérer cette autorité d'intégration",
        "diese Integrationsautorität freigeben",
        "lepaskan otoritas integrasi tersebut",
        "释放该集成权限",
        "その連携権限を解放",
        "해당 통합 권한 해제"
    ),
    tr!(
        "list resumable sessions found on disk",
        "listar sesiones reanudables encontradas en disco",
        "listar sessões retomáveis encontradas no disco",
        "lister les sessions reprenables trouvées sur le disque",
        "fortsetzbare Sitzungen auf dem Datenträger auflisten",
        "daftar sesi yang dapat dilanjutkan dari disk",
        "列出磁盘上可恢复的会话",
        "ディスク上の再開可能なセッションを一覧表示",
        "디스크에서 발견된 재개 가능한 세션 나열"
    ),
    tr!(
        "reopen a resumable session into a pane",
        "reabrir una sesión reanudable en un panel",
        "reabrir uma sessão retomável em um painel",
        "rouvrir une session reprenable dans un volet",
        "fortsetzbare Sitzung in einem Bereich öffnen",
        "buka kembali sesi yang dapat dilanjutkan ke panel",
        "在窗格中重新打开可恢复会话",
        "再開可能なセッションをペインで開く",
        "재개 가능한 세션을 패널에서 다시 열기"
    ),
    tr!(
        "install the bundled skill in detected agent hosts",
        "instalar la habilidad incluida en los agentes detectados",
        "instalar a skill incluída nos agentes detectados",
        "installer la compétence intégrée dans les hôtes d'agents détectés",
        "gebündelten Skill in erkannten Agenten-Hosts installieren",
        "pasang skill bawaan di host agen yang terdeteksi",
        "在检测到的智能体宿主中安装内置技能",
        "検出したエージェントホストへ同梱スキルをインストール",
        "감지된 에이전트 호스트에 번들 스킬 설치"
    ),
    tr!(
        "show the bundled release and installation details",
        "mostrar la versión incluida y los detalles de instalación",
        "mostrar a versão incluída e detalhes de instalação",
        "afficher la version intégrée et les détails d'installation",
        "gebündelte Version und Installationsdetails anzeigen",
        "tampilkan rilis bawaan dan detail instalasi",
        "显示内置版本和安装详情",
        "同梱リリースとインストール詳細を表示",
        "번들 릴리스와 설치 세부 정보 표시"
    ),
    tr!(
        "remove unchanged Luvus-managed installations",
        "eliminar instalaciones sin cambios gestionadas por Luvus",
        "remover instalações inalteradas gerenciadas pelo Luvus",
        "supprimer les installations Luvus non modifiées",
        "unveränderte Luvus-verwaltete Installationen entfernen",
        "hapus instalasi kelolaan Luvus yang tidak berubah",
        "删除未修改的 Luvus 管理安装",
        "未変更の Luvus 管理インストールを削除",
        "변경되지 않은 Luvus 관리 설치 제거"
    ),
    tr!(
        "print the bundled, version-matched SKILL.md",
        "mostrar el SKILL.md incluido y compatible con la versión",
        "exibir o SKILL.md incluído e compatível com a versão",
        "afficher le SKILL.md intégré correspondant à la version",
        "gebündelte, versionspassende SKILL.md ausgeben",
        "tampilkan SKILL.md bawaan yang cocok dengan versi",
        "输出内置且版本匹配的 SKILL.md",
        "同梱されたバージョン一致の SKILL.md を表示",
        "버전이 일치하는 번들 SKILL.md 출력"
    ),
    tr!(
        "block until output appears",
        "bloquear hasta que aparezca la salida",
        "bloquear até a saída aparecer",
        "bloquer jusqu'à l'apparition de la sortie",
        "blockieren, bis Ausgabe erscheint",
        "blokir hingga keluaran muncul",
        "阻塞直到出现输出",
        "出力が現れるまで待機",
        "출력이 나타날 때까지 대기"
    ),
    tr!(
        "open the TUI into a single fullscreen pane",
        "abrir la TUI en un único panel a pantalla completa",
        "abrir a TUI em um único painel em tela cheia",
        "ouvrir la TUI dans un seul volet plein écran",
        "TUI in einem einzelnen Vollbildbereich öffnen",
        "buka TUI dalam satu panel layar penuh",
        "在单个全屏窗格中打开 TUI",
        "単一の全画面ペインで TUI を開く",
        "TUI를 패널 하나의 전체 화면으로 열기"
    ),
    tr!(
        "find text across every pane's scrollback (docs/63);",
        "buscar texto en el historial de todos los paneles (docs/63);",
        "buscar texto no histórico de todos os painéis (docs/63);",
        "rechercher du texte dans l'historique de tous les volets (docs/63) ;",
        "Text im Verlauf aller Bereiche suchen (docs/63);",
        "cari teks di riwayat semua panel (docs/63);",
        "在所有窗格的回滚历史中查找文本（docs/63）；",
        "すべてのペインのスクロールバックからテキストを検索（docs/63）；",
        "모든 패널의 스크롤백에서 텍스트 찾기 (docs/63);"
    ),
    tr!(
        "--case is case-sensitive; returns matches as JSON",
        "--case distingue mayúsculas; devuelve coincidencias como JSON",
        "--case diferencia maiúsculas; retorna correspondências como JSON",
        "--case respecte la casse ; renvoie les résultats en JSON",
        "--case beachtet Groß-/Kleinschreibung; Treffer als JSON",
        "--case peka huruf besar-kecil; hasil berupa JSON",
        "--case 区分大小写；匹配结果以 JSON 返回",
        "--case は大文字小文字を区別し、結果を JSON で返す",
        "--case는 대소문자 구분; 일치 결과를 JSON으로 반환"
    ),
    tr!(
        "rank navigation, file paths, and retained output;",
        "clasificar navegación, rutas de archivos y salida conservada;",
        "classificar navegação, caminhos e saída retida;",
        "classer la navigation, les chemins et la sortie conservée ;",
        "Navigation, Dateipfade und gespeicherte Ausgabe bewerten;",
        "urutkan navigasi, path berkas, dan keluaran tersimpan;",
        "对导航、文件路径和保留输出进行排序；",
        "ナビゲーション、ファイルパス、保持済み出力を順位付け；",
        "탐색 항목, 파일 경로 및 보존된 출력의 순위를 매김;"
    ),
    tr!(
        "legacy search stays exact unless --fuzzy is passed",
        "la búsqueda anterior sigue siendo exacta salvo que se use --fuzzy",
        "a busca anterior permanece exata salvo com --fuzzy",
        "la recherche historique reste exacte sauf avec --fuzzy",
        "die bisherige Suche bleibt exakt, außer mit --fuzzy",
        "pencarian lama tetap persis kecuali memakai --fuzzy",
        "除非传入 --fuzzy，旧搜索仍为精确匹配",
        "--fuzzy を指定しない限り従来の検索は完全一致",
        "--fuzzy를 지정하지 않으면 기존 검색은 정확히 일치"
    ),
    tr!(
        "list built-in, installed, and virtual themes",
        "listar temas integrados, instalados y virtuales",
        "listar temas integrados, instalados e virtuais",
        "lister les thèmes intégrés, installés et virtuels",
        "integrierte, installierte und virtuelle Themes auflisten",
        "daftar tema bawaan, terpasang, dan virtual",
        "列出内置、已安装和虚拟主题",
        "組み込み、インストール済み、仮想テーマを一覧表示",
        "내장, 설치됨, 가상 테마 나열"
    ),
    tr!(
        "print/create the shared themes directory",
        "mostrar/crear el directorio compartido de temas",
        "exibir/criar o diretório compartilhado de temas",
        "afficher/créer le répertoire partagé des thèmes",
        "gemeinsames Theme-Verzeichnis ausgeben/erstellen",
        "tampilkan/buat direktori tema bersama",
        "输出/创建共享主题目录",
        "共有テーマディレクトリを表示/作成",
        "공유 테마 디렉터리 출력/생성"
    ),
    tr!(
        "write an editable TOML starter",
        "crear una plantilla TOML editable",
        "gravar um modelo TOML editável",
        "écrire un modèle TOML modifiable",
        "editierbare TOML-Vorlage schreiben",
        "tulis template TOML yang dapat diedit",
        "写入可编辑的 TOML 模板",
        "編集可能な TOML 雛形を書き出す",
        "편집 가능한 TOML 시작 파일 작성"
    ),
    tr!(
        "validate without installing",
        "validar sin instalar",
        "validar sem instalar",
        "valider sans installer",
        "ohne Installation prüfen",
        "validasi tanpa memasang",
        "验证但不安装",
        "インストールせず検証",
        "설치하지 않고 검증"
    ),
    tr!(
        "install a local file, HTTPS URL, GitHub repo, or community/<id>",
        "instalar un archivo local, URL HTTPS, repositorio GitHub o community/<id>",
        "instalar arquivo local, URL HTTPS, repositório GitHub ou community/<id>",
        "installer un fichier local, une URL HTTPS, un dépôt GitHub ou community/<id>",
        "lokale Datei, HTTPS-URL, GitHub-Repository oder community/<id> installieren",
        "pasang berkas lokal, URL HTTPS, repo GitHub, atau community/<id>",
        "安装本地文件、HTTPS URL、GitHub 仓库或 community/<id>",
        "ローカルファイル、HTTPS URL、GitHub リポジトリ、community/<id> をインストール",
        "로컬 파일, HTTPS URL, GitHub 저장소, 또는 community/<id> 설치"
    ),
    tr!(
        "select and persist a registered theme",
        "seleccionar y guardar un tema registrado",
        "selecionar e salvar um tema registrado",
        "sélectionner et conserver un thème enregistré",
        "registriertes Theme auswählen und speichern",
        "pilih dan simpan tema terdaftar",
        "选择并保存已注册主题",
        "登録済みテーマを選択して保存",
        "등록된 테마 선택 및 저장"
    ),
    tr!(
        "remove an inactive local theme",
        "eliminar un tema local inactivo",
        "remover um tema local inativo",
        "supprimer un thème local inactif",
        "inaktives lokales Theme entfernen",
        "hapus tema lokal yang tidak aktif",
        "删除未启用的本地主题",
        "未使用のローカルテーマを削除",
        "비활성 로컬 테마 제거"
    ),
    tr!(
        "rescan installed themes in the selected server",
        "volver a analizar los temas instalados en el servidor seleccionado",
        "reexaminar temas instalados no servidor selecionado",
        "réanalyser les thèmes installés sur le serveur sélectionné",
        "installierte Themes im ausgewählten Server neu einlesen",
        "pindai ulang tema terpasang di server terpilih",
        "在所选服务器中重新扫描已安装主题",
        "選択したサーバーでインストール済みテーマを再スキャン",
        "선택한 서버에서 설치된 테마 다시 검색"
    ),
    tr!(
        "list declared Luvus Bar widgets and live content",
        "listar widgets declarados de Luvus Bar y contenido activo",
        "listar widgets declarados da Luvus Bar e conteúdo ativo",
        "lister les widgets Luvus Bar déclarés et leur contenu actif",
        "deklarierte Luvus-Bar-Widgets und Live-Inhalte auflisten",
        "daftar widget Luvus Bar dan konten langsung",
        "列出已声明的 Luvus Bar 组件和实时内容",
        "宣言済み Luvus Bar ウィジェットとライブ内容を一覧表示",
        "선언된 Luvus Bar 위젯과 실시간 콘텐츠 나열"
    ),
    tr!(
        "publish validated live widget segments;",
        "publicar segmentos de widget activos y validados;",
        "publicar segmentos validados de widget ao vivo;",
        "publier des segments de widget actifs validés ;",
        "geprüfte Live-Widget-Segmente veröffentlichen;",
        "terbitkan segmen widget langsung yang tervalidasi;",
        "发布已验证的实时组件片段；",
        "検証済みライブウィジェットセグメントを公開；",
        "검증된 실시간 위젯 세그먼트 게시;"
    ),
    tr!(
        "--content-file, --compact-content, --text and --state supported",
        "admite --content-file, --compact-content, --text y --state",
        "suporta --content-file, --compact-content, --text e --state",
        "prend en charge --content-file, --compact-content, --text et --state",
        "--content-file, --compact-content, --text und --state werden unterstützt",
        "mendukung --content-file, --compact-content, --text, dan --state",
        "支持 --content-file、--compact-content、--text 和 --state",
        "--content-file、--compact-content、--text、--state に対応",
        "--content-file, --compact-content, --text, --state 지원"
    ),
    tr!(
        "clear live widget content, preserving placement",
        "borrar contenido activo conservando la posición",
        "limpar conteúdo ativo preservando a posição",
        "effacer le contenu actif en conservant l'emplacement",
        "Live-Widget-Inhalt löschen, Platzierung beibehalten",
        "hapus konten langsung, pertahankan penempatan",
        "清除实时组件内容并保留位置",
        "配置位置を保ったままライブ内容を消去",
        "배치는 유지한 채 실시간 위젯 콘텐츠 지우기"
    ),
    tr!(
        "set a sidebar's width (columns)",
        "definir el ancho de una barra lateral (columnas)",
        "definir a largura de uma barra lateral (colunas)",
        "définir la largeur d'une barre latérale (colonnes)",
        "Breite einer Seitenleiste festlegen (Spalten)",
        "atur lebar bilah sisi (kolom)",
        "设置侧边栏宽度（列）",
        "サイドバー幅を設定（列）",
        "사이드바 너비 설정 (열 단위)"
    ),
    tr!(
        "toggle a sidebar",
        "mostrar u ocultar una barra lateral",
        "alternar uma barra lateral",
        "afficher ou masquer une barre latérale",
        "Seitenleiste ein-/ausblenden",
        "tampilkan/sembunyikan bilah sisi",
        "显示或隐藏侧边栏",
        "サイドバーを表示/非表示",
        "사이드바 전환"
    ),
    tr!(
        "list docks and which side each is on",
        "listar docks y el lado de cada uno",
        "listar docks e o lado de cada um",
        "lister les docks et leur côté",
        "Docks und ihre jeweilige Seite auflisten",
        "daftar dock dan sisinya",
        "列出停靠栏及其所在侧",
        "ドックと配置側を一覧表示",
        "도킹과 각 도킹의 배치 위치 나열"
    ),
    tr!(
        "place a dock on a side",
        "colocar un dock en un lado",
        "posicionar um dock em um lado",
        "placer un dock sur un côté",
        "Dock auf einer Seite platzieren",
        "tempatkan dock di satu sisi",
        "将停靠栏放置在一侧",
        "ドックを左右どちらかに配置",
        "도킹을 한쪽에 배치"
    ),
    tr!(
        "feed a module's sidebar dock its rows (JSON array,",
        "enviar filas al dock lateral de un módulo (matriz JSON,",
        "enviar linhas ao dock lateral de um módulo (array JSON,",
        "fournir les lignes au dock latéral d'un module (tableau JSON,",
        "Zeilen an das Seitenleisten-Dock eines Moduls senden (JSON-Array,",
        "kirim baris ke dock bilah sisi modul (array JSON,",
        "向模块侧边停靠栏提供行数据（JSON 数组，",
        "モジュールのサイドバードックへ行を送信（JSON 配列、",
        "모듈의 사이드바 도킹에 행 전달 (JSON 배열,"
    ),
    tr!(
        "or piped on stdin). See docs/29 + the website",
        "o por stdin). Consulta docs/29 y el sitio web",
        "ou via stdin). Veja docs/29 e o site",
        "ou via stdin). Voir docs/29 et le site",
        "oder über stdin). Siehe docs/29 und Website",
        "atau lewat stdin). Lihat docs/29 dan situs",
        "或通过 stdin 管道传入）。参见 docs/29 和网站",
        "または stdin）。docs/29 とウェブサイトを参照",
        "또는 stdin으로 파이프). docs/29 및 웹사이트 참고"
    ),
    tr!(
        "flash a one-line message in the UI",
        "mostrar brevemente un mensaje de una línea en la interfaz",
        "exibir brevemente uma mensagem de uma linha na interface",
        "afficher brièvement un message d'une ligne dans l'interface",
        "einzeilige Meldung kurz in der UI anzeigen",
        "tampilkan pesan satu baris sejenak di UI",
        "在界面中短暂显示单行消息",
        "UI に1行メッセージを一時表示",
        "UI에 한 줄 메시지 잠깐 표시"
    ),
    tr!(
        "find modules published to the `luvus-module` GitHub topic",
        "buscar módulos publicados en el tema `luvus-module` de GitHub",
        "buscar módulos publicados no tópico `luvus-module` do GitHub",
        "trouver les modules publiés sous le sujet GitHub `luvus-module`",
        "unter dem GitHub-Topic `luvus-module` veröffentlichte Module finden",
        "cari modul di topik GitHub `luvus-module`",
        "查找发布到 GitHub `luvus-module` 主题的模块",
        "GitHub の `luvus-module` トピックで公開されたモジュールを検索",
        "`luvus-module` GitHub 토픽에 게시된 모듈 찾기"
    ),
    tr!(
        "list installed modules",
        "listar módulos instalados",
        "listar módulos instalados",
        "lister les modules installés",
        "installierte Module auflisten",
        "daftar modul terpasang",
        "列出已安装模块",
        "インストール済みモジュールを一覧表示",
        "설치된 모듈 나열"
    ),
    tr!(
        "show a module's actions / panes / events / source",
        "mostrar acciones / paneles / eventos / origen de un módulo",
        "mostrar ações / painéis / eventos / origem de um módulo",
        "afficher les actions / volets / événements / source d'un module",
        "Aktionen / Bereiche / Ereignisse / Quelle eines Moduls anzeigen",
        "tampilkan aksi / panel / peristiwa / sumber modul",
        "显示模块的操作 / 窗格 / 事件 / 来源",
        "モジュールのアクション / ペイン / イベント / ソースを表示",
        "모듈의 동작/패널/이벤트/소스 표시"
    ),
    tr!(
        "register a local module dir (--disabled to skip enabling)",
        "registrar un directorio de módulo local (--disabled evita activarlo)",
        "registrar diretório de módulo local (--disabled não ativa)",
        "enregistrer un répertoire de module local (--disabled évite l'activation)",
        "lokales Modulverzeichnis registrieren (--disabled aktiviert es nicht)",
        "daftarkan direktori modul lokal (--disabled agar tidak diaktifkan)",
        "注册本地模块目录（--disabled 跳过启用）",
        "ローカルモジュールディレクトリを登録（--disabled で無効のまま）",
        "로컬 모듈 디렉터리 등록 (--disabled로 활성화 건너뛰기)"
    ),
    tr!(
        "install from GitHub",
        "instalar desde GitHub",
        "instalar do GitHub",
        "installer depuis GitHub",
        "von GitHub installieren",
        "pasang dari GitHub",
        "从 GitHub 安装",
        "GitHub からインストール",
        "GitHub에서 설치"
    ),
    tr!(
        "remove a module from the registry",
        "eliminar un módulo del registro",
        "remover um módulo do registro",
        "retirer un module du registre",
        "Modul aus der Registrierung entfernen",
        "hapus modul dari registry",
        "从注册表中移除模块",
        "モジュールをレジストリから削除",
        "레지스트리에서 모듈 제거"
    ),
    tr!(
        "unlink + delete a git-installed module's checkout",
        "desvincular y borrar el checkout de un módulo instalado con git",
        "desvincular e excluir o checkout de um módulo instalado via git",
        "dissocier et supprimer le checkout d'un module installé par git",
        "Verknüpfung und Checkout eines per Git installierten Moduls löschen",
        "putuskan dan hapus checkout modul yang dipasang lewat git",
        "取消链接并删除通过 git 安装的模块检出",
        "リンク解除し Git インストール済みモジュールのチェックアウトを削除",
        "git으로 설치된 모듈의 체크아웃 연결 해제 및 삭제"
    ),
    tr!(
        "<id> is a module id or the owner/repo it came from",
        "<id> es un id de módulo o su owner/repo de origen",
        "<id> é um id de módulo ou seu owner/repo de origem",
        "<id> est un identifiant de module ou son owner/repo d'origine",
        "<id> ist eine Modul-ID oder das ursprüngliche owner/repo",
        "<id> adalah id modul atau owner/repo asalnya",
        "<id> 是模块 ID 或其来源 owner/repo",
        "<id> はモジュール ID または取得元 owner/repo",
        "<id>는 모듈 id 또는 출처인 owner/repo"
    ),
    tr!(
        "list every action across modules",
        "listar todas las acciones de los módulos",
        "listar todas as ações dos módulos",
        "lister toutes les actions des modules",
        "alle Aktionen aller Module auflisten",
        "daftar semua aksi lintas modul",
        "列出所有模块操作",
        "全モジュールのアクションを一覧表示",
        "모든 모듈의 동작 나열"
    ),
    tr!(
        "invoke a module action (captures + logs output)",
        "invocar una acción de módulo (captura y registra la salida)",
        "invocar uma ação de módulo (captura e registra a saída)",
        "exécuter une action de module (capture et journalise la sortie)",
        "Modulaktion ausführen (Ausgabe erfassen und protokollieren)",
        "jalankan aksi modul (tangkap dan catat keluaran)",
        "调用模块操作（捕获并记录输出）",
        "モジュールアクションを実行（出力を取得、記録）",
        "모듈 동작 실행 (출력 캡처 및 로그 기록)"
    ),
    tr!(
        "tail module command logs (--limit N)",
        "seguir registros de comandos del módulo (--limit N)",
        "acompanhar logs de comandos do módulo (--limit N)",
        "suivre les journaux de commandes du module (--limit N)",
        "Modul-Befehlsprotokolle verfolgen (--limit N)",
        "ikuti log perintah modul (--limit N)",
        "查看模块命令日志末尾（--limit N）",
        "モジュールコマンドログを追跡（--limit N）",
        "모듈 명령 로그 실시간 확인 (--limit N)"
    ),
    tr!(
        "print/create a module's config dir",
        "mostrar/crear el directorio de configuración de un módulo",
        "exibir/criar o diretório de configuração de um módulo",
        "afficher/créer le répertoire de configuration d'un module",
        "Konfigurationsverzeichnis eines Moduls ausgeben/erstellen",
        "tampilkan/buat direktori konfigurasi modul",
        "输出/创建模块配置目录",
        "モジュール設定ディレクトリを表示/作成",
        "모듈 설정 디렉터리 출력/생성"
    ),
    tr!(
        "list a module's declared settings and values",
        "listar ajustes y valores declarados de un módulo",
        "listar configurações e valores declarados de um módulo",
        "lister les réglages déclarés et leurs valeurs",
        "deklarierte Einstellungen und Werte eines Moduls auflisten",
        "daftar pengaturan dan nilai modul",
        "列出模块声明的设置和值",
        "モジュールが宣言した設定と値を一覧表示",
        "모듈에 선언된 설정과 값 나열"
    ),
    tr!(
        "read / write one setting",
        "leer / escribir un ajuste",
        "ler / gravar uma configuração",
        "lire / écrire un réglage",
        "eine Einstellung lesen / schreiben",
        "baca / tulis satu pengaturan",
        "读取 / 写入一项设置",
        "設定を1つ読み書き",
        "설정 하나 읽기/쓰기"
    ),
    tr!(
        "branch, ahead/behind, working tree of the current workspace",
        "rama, avance/retraso y árbol de trabajo del espacio actual",
        "branch, avanço/atraso e árvore de trabalho do espaço atual",
        "branche, avance/retard et arbre de travail de l'espace actuel",
        "Branch, voraus/hinterher und Arbeitsbaum des aktuellen Arbeitsbereichs",
        "branch, ahead/behind, dan working tree ruang kerja saat ini",
        "当前工作区的分支、领先/落后和工作树",
        "現在のワークスペースのブランチ、ahead/behind、作業ツリー",
        "현재 작업 공간의 브랜치, 앞섬/뒤처짐, 작업 트리"
    ),
    tr!(
        "local branches with tracking",
        "ramas locales con seguimiento",
        "branches locais com rastreamento",
        "branches locales avec suivi",
        "lokale Branches mit Tracking",
        "branch lokal dengan tracking",
        "带跟踪信息的本地分支",
        "追跡情報付きローカルブランチ",
        "추적 정보가 있는 로컬 브랜치"
    ),
    tr!(
        "recent commits",
        "commits recientes",
        "commits recentes",
        "commits récents",
        "letzte Commits",
        "commit terbaru",
        "最近提交",
        "最近のコミット",
        "최근 커밋"
    ),
    tr!(
        "open the git tab for a workspace",
        "abrir la pestaña Git de un espacio de trabajo",
        "abrir a aba Git de um espaço de trabalho",
        "ouvrir l'onglet Git d'un espace de travail",
        "Git-Tab für einen Arbeitsbereich öffnen",
        "buka tab Git untuk ruang kerja",
        "打开工作区的 Git 标签页",
        "ワークスペースの Git タブを開く",
        "작업 공간의 git 탭 열기"
    ),
    tr!(
        "open Mission Control for a workspace",
        "abrir Control de Misión para un espacio de trabajo",
        "abrir Controle de Missão para um espaço de trabalho",
        "ouvrir le Contrôle de Mission pour un espace de travail",
        "Missionskontrolle für einen Arbeitsbereich öffnen",
        "buka Kontrol Misi untuk ruang kerja",
        "为工作区打开任务控制",
        "ワークスペースのミッションコントロールを開く",
        "작업 공간의 미션 컨트롤 열기"
    ),
    tr!(
        "print the FILES tree of the active node",
        "mostrar el árbol FILES del nodo activo",
        "exibir a árvore FILES do nó ativo",
        "afficher l'arborescence FILES du nœud actif",
        "FILES-Baum des aktiven Knotens ausgeben",
        "tampilkan pohon FILES node aktif",
        "输出活动节点的 FILES 树",
        "アクティブノードの FILES ツリーを表示",
        "활성 노드의 FILES 트리 출력"
    ),
    tr!(
        "open a file in a view",
        "abrir un archivo en una vista",
        "abrir um arquivo em uma visualização",
        "ouvrir un fichier dans une vue",
        "Datei in einer Ansicht öffnen",
        "buka berkas dalam tampilan",
        "在视图中打开文件",
        "ファイルをビューで開く",
        "보기에서 파일 열기"
    ),
    tr!(
        "expand the tree to a path",
        "expandir el árbol hasta una ruta",
        "expandir a árvore até um caminho",
        "développer l'arborescence jusqu'à un chemin",
        "Baum bis zu einem Pfad aufklappen",
        "bentangkan pohon hingga suatu path",
        "将树展开到指定路径",
        "指定パスまでツリーを展開",
        "경로까지 트리 펼치기"
    ),
    tr!(
        "re-read the tree from disk",
        "volver a leer el árbol desde el disco",
        "reler a árvore do disco",
        "relire l'arborescence depuis le disque",
        "Baum erneut vom Datenträger lesen",
        "baca ulang pohon dari disk",
        "从磁盘重新读取树",
        "ディスクからツリーを再読み込み",
        "디스크에서 트리 다시 읽기"
    ),
    tr!(
        "list exact diff layers",
        "listar capas exactas del diff",
        "listar camadas exatas do diff",
        "lister les couches exactes du diff",
        "exakte Diff-Ebenen auflisten",
        "daftar lapisan diff yang tepat",
        "列出精确差异层",
        "正確な差分レイヤーを一覧表示",
        "정확한 diff 레이어 나열"
    ),
    tr!(
        "inspect a bounded semantic diff",
        "inspeccionar un diff semántico limitado",
        "inspecionar um diff semântico limitado",
        "inspecter un diff sémantique borné",
        "begrenzten semantischen Diff prüfen",
        "periksa diff semantik terbatas",
        "检查有界语义差异",
        "範囲制限されたセマンティック差分を確認",
        "범위가 제한된 의미 단위 diff 검사"
    ),
    tr!(
        "refresh the shared FILES and DIFF index",
        "actualizar el índice compartido FILES y DIFF",
        "atualizar o índice compartilhado FILES e DIFF",
        "actualiser l'index partagé FILES et DIFF",
        "gemeinsamen FILES- und DIFF-Index aktualisieren",
        "perbarui indeks FILES dan DIFF bersama",
        "刷新共享 FILES 和 DIFF 索引",
        "共有 FILES / DIFF インデックスを更新",
        "공유 FILES 및 DIFF 색인 새로고침"
    ),
    tr!(
        "list the current repo's worktrees",
        "listar los worktrees del repositorio actual",
        "listar os worktrees do repositório atual",
        "lister les worktrees du dépôt actuel",
        "Worktrees des aktuellen Repositorys auflisten",
        "daftar worktree repo saat ini",
        "列出当前仓库的工作树",
        "現在のリポジトリのワークツリーを一覧表示",
        "현재 저장소의 worktree 나열"
    ),
    tr!(
        "create a worktree + workspace for <branch>",
        "crear un worktree y espacio de trabajo para <branch>",
        "criar worktree e espaço de trabalho para <branch>",
        "créer un worktree et un espace de travail pour <branch>",
        "Worktree und Arbeitsbereich für <branch> erstellen",
        "buat worktree dan ruang kerja untuk <branch>",
        "为 <branch> 创建工作树和工作区",
        "<branch> 用のワークツリーとワークスペースを作成",
        "<branch>용 worktree + 작업 공간 생성"
    ),
    tr!(
        "open an existing worktree as a workspace",
        "abrir un worktree existente como espacio de trabajo",
        "abrir um worktree existente como espaço de trabalho",
        "ouvrir un worktree existant comme espace de travail",
        "vorhandenen Worktree als Arbeitsbereich öffnen",
        "buka worktree yang ada sebagai ruang kerja",
        "将现有工作树作为工作区打开",
        "既存ワークツリーをワークスペースとして開く",
        "기존 worktree를 작업 공간으로 열기"
    ),
    tr!(
        "remove a worktree (its branch is kept)",
        "eliminar un worktree (se conserva su rama)",
        "remover um worktree (a branch é mantida)",
        "supprimer un worktree (sa branche est conservée)",
        "Worktree entfernen (Branch bleibt erhalten)",
        "hapus worktree (branch tetap disimpan)",
        "删除工作树（保留其分支）",
        "ワークツリーを削除（ブランチは保持）",
        "worktree 제거 (브랜치는 유지)"
    ),
    tr!(
        "list all tasks + their status/assignee",
        "listar todas las tareas, estado y responsable",
        "listar todas as tarefas, status e responsável",
        "lister toutes les tâches, leur état et responsable",
        "alle Aufgaben mit Status und Zuständigem auflisten",
        "daftar semua tugas, status, dan penanggung jawab",
        "列出所有任务及其状态/负责人",
        "全タスクと状態/担当者を一覧表示",
        "모든 작업과 상태/담당자 나열"
    ),
    tr!(
        "show one task",
        "mostrar una tarea",
        "mostrar uma tarefa",
        "afficher une tâche",
        "eine Aufgabe anzeigen",
        "tampilkan satu tugas",
        "显示一个任务",
        "タスクを1つ表示",
        "작업 하나 표시"
    ),
    tr!(
        "claim a task for this pane (deps must be done)",
        "asignar una tarea a este panel (dependencias terminadas)",
        "assumir uma tarefa neste painel (dependências concluídas)",
        "réserver une tâche pour ce volet (dépendances terminées)",
        "Aufgabe für diesen Bereich übernehmen (Abhängigkeiten müssen erledigt sein)",
        "ambil tugas untuk panel ini (dependensi harus selesai)",
        "为此窗格领取任务（依赖必须完成）",
        "このペインでタスクを担当（依存タスクは完了必須）",
        "이 패널에 작업 할당 (의존 작업이 완료되어야 함)"
    ),
    tr!(
        "claim the next ready task (--start creates a worker)",
        "asignar la siguiente tarea lista (--start crea un trabajador)",
        "assumir a próxima tarefa pronta (--start cria um worker)",
        "réserver la prochaine tâche prête (--start crée un worker)",
        "nächste bereite Aufgabe übernehmen (--start erstellt einen Worker)",
        "ambil tugas siap berikutnya (--start membuat worker)",
        "领取下一个就绪任务（--start 创建工作进程）",
        "次の実行可能タスクを担当（--start でワーカーを作成）",
        "준비된 다음 작업 할당 (--start로 워커 생성)"
    ),
    tr!(
        "start a worker (worktree default; workspace shares checkout)",
        "iniciar un trabajador (worktree predeterminado; workspace comparte el checkout)",
        "iniciar um worker (worktree padrão; workspace compartilha o checkout)",
        "démarrer un worker (worktree par défaut ; workspace partage le checkout)",
        "Worker starten (Worktree ist Standard; Workspace teilt den Checkout)",
        "jalankan worker (default worktree; workspace berbagi checkout)",
        "启动工作进程（默认 worktree；workspace 共享检出目录）",
        "ワーカーを起動（既定は worktree、workspace はチェックアウトを共有）",
        "워커 시작 (기본값 worktree, workspace는 체크아웃 공유)"
    ),
    tr!(
        "report context usage (blocks done at >85%)",
        "informar uso de contexto (bloquea finalizar por encima del 85%)",
        "informar uso de contexto (bloqueia conclusão acima de 85%)",
        "signaler l'usage du contexte (bloque la fin au-delà de 85 %)",
        "Kontextnutzung melden (blockiert Abschluss über 85 %)",
        "laporkan penggunaan konteks (blokir selesai di atas 85%)",
        "报告上下文使用率（超过 85% 时阻止完成）",
        "コンテキスト使用率を報告（85%超で完了を拒否）",
        "컨텍스트 사용량 보고 (85% 초과 시 완료 차단)"
    ),
    tr!(
        "mark done + release its leases",
        "marcar como terminada y liberar sus reservas",
        "marcar como concluída e liberar reservas",
        "marquer terminée et libérer ses réservations",
        "als erledigt markieren und Reservierungen freigeben",
        "tandai selesai dan lepaskan sewanya",
        "标记完成并释放租约",
        "完了にして予約を解放",
        "완료 표시 및 임대 해제"
    ),
    tr!(
        "integrate the task's branch into luvus/integration",
        "integrar la rama de la tarea en luvus/integration",
        "integrar a branch da tarefa em luvus/integration",
        "intégrer la branche de la tâche dans luvus/integration",
        "Aufgaben-Branch in luvus/integration integrieren",
        "integrasikan branch tugas ke luvus/integration",
        "将任务分支集成到 luvus/integration",
        "タスクのブランチを luvus/integration へ統合",
        "작업 브랜치를 luvus/integration에 통합"
    ),
    tr!(
        "(isolated worktree, conflicts block the task)",
        "(worktree aislado, los conflictos bloquean la tarea)",
        "(worktree isolado, conflitos bloqueiam a tarefa)",
        "(worktree isolé, les conflits bloquent la tâche)",
        "(isolierter Worktree, Konflikte blockieren die Aufgabe)",
        "(worktree terisolasi, konflik memblokir tugas)",
        "（隔离工作树，冲突会阻塞任务）",
        "（分離ワークツリー、競合時はタスクをブロック）",
        "(격리된 worktree, 충돌 시 작업이 차단됨)"
    ),
    tr!(
        "return a claimed task to the queue",
        "devolver una tarea asignada a la cola",
        "devolver uma tarefa assumida à fila",
        "remettre une tâche réservée dans la file",
        "übernommene Aufgabe in die Warteschlange zurückgeben",
        "kembalikan tugas yang diambil ke antrean",
        "将已领取任务退回队列",
        "担当中タスクをキューへ戻す",
        "할당된 작업을 큐로 반환"
    ),
    tr!(
        "remove a task (release/finish an active one first)",
        "eliminar una tarea (liberar/finalizar antes si está activa)",
        "remover uma tarefa (libere/conclua antes se estiver ativa)",
        "supprimer une tâche (libérer/terminer d'abord si active)",
        "Aufgabe entfernen (aktive zuerst freigeben/abschließen)",
        "hapus tugas (lepaskan/selesaikan yang aktif dulu)",
        "删除任务（活动任务须先释放/完成）",
        "タスクを削除（実行中なら先に解放/完了）",
        "작업 제거 (활성 작업은 먼저 해제/완료 필요)"
    ),
    tr!(
        "reserve paths for an unfinished task",
        "reservar rutas para una tarea sin terminar",
        "reservar caminhos para uma tarefa não concluída",
        "réserver des chemins pour une tâche non terminée",
        "Pfade für eine nicht abgeschlossene Aufgabe reservieren",
        "pesan path untuk tugas yang belum selesai",
        "为未完成的任务预留路径",
        "未完了のタスク用にパスを予約",
        "완료되지 않은 작업의 경로 예약"
    ),
    tr!(
        "(denied if they overlap another task)",
        "(se rechaza si se solapan con otra tarea)",
        "(negado se houver sobreposição com outra tarefa)",
        "(refusé en cas de chevauchement avec une autre tâche)",
        "(abgelehnt bei Überschneidung mit einer anderen Aufgabe)",
        "(ditolak jika tumpang tindih dengan tugas lain)",
        "（与其他任务重叠时拒绝）",
        "（別のタスクと重複する場合は拒否）",
        "(다른 작업과 겹치면 거부됨)"
    ),
    tr!(
        "release a lease",
        "liberar una reserva",
        "liberar uma reserva",
        "libérer une réservation",
        "Reservierung freigeben",
        "lepaskan sewa",
        "释放租约",
        "予約を解放",
        "임대 해제"
    ),
    tr!(
        "list active path leases",
        "listar reservas de rutas activas",
        "listar reservas de caminhos ativas",
        "lister les réservations de chemins actives",
        "aktive Pfadreservierungen auflisten",
        "daftar sewa path aktif",
        "列出活动路径租约",
        "有効なパス予約を一覧表示",
        "활성 경로 임대 나열"
    ),
    tr!(
        "stream live status changes",
        "transmitir cambios de estado en vivo",
        "transmitir mudanças de status ao vivo",
        "diffuser les changements d'état en direct",
        "Live-Statusänderungen streamen",
        "alirkan perubahan status langsung",
        "流式输出实时状态变化",
        "状態変更をリアルタイム配信",
        "실시간 상태 변경 스트리밍"
    ),
    tr!(
        "print live methods, contracts, limits, and protocol identity",
        "mostrar métodos, contratos, límites e identidad del protocolo",
        "exibir métodos, contratos, limites e identidade do protocolo",
        "afficher les méthodes, contrats, limites et l'identité du protocole",
        "Live-Methoden, Verträge, Grenzen und Protokollidentität ausgeben",
        "tampilkan metode, kontrak, batas, dan identitas protokol",
        "输出实时方法、契约、限制和协议身份",
        "ライブメソッド、契約、制限、プロトコル識別を表示",
        "실시간 메서드, 계약, 제한, 프로토콜 식별 정보 출력"
    ),
    tr!(
        "print the complete installed UHP JSON Schema bundle",
        "mostrar el paquete completo de esquemas JSON UHP instalado",
        "exibir o pacote completo de JSON Schema UHP instalado",
        "afficher le paquet JSON Schema UHP installé complet",
        "vollständiges installiertes UHP-JSON-Schema-Bundle ausgeben",
        "tampilkan bundel JSON Schema UHP terpasang lengkap",
        "输出完整的已安装 UHP JSON Schema 包",
        "インストール済み UHP JSON Schema 一式を表示",
        "설치된 전체 UHP JSON 스키마 번들 출력"
    ),
    tr!(
        "print a fenced session snapshot for harness bootstrap",
        "mostrar una instantánea delimitada para iniciar el harness",
        "exibir snapshot delimitado para iniciar o harness",
        "afficher un instantané délimité pour amorcer le harness",
        "abgegrenzten Sitzungssnapshot für Harness-Bootstrap ausgeben",
        "tampilkan snapshot sesi berpagar untuk bootstrap harness",
        "输出用于框架引导的有界会话快照",
        "ハーネス起動用の区切られたセッションスナップショットを表示",
        "하네스 부트스트랩용 구분된 세션 스냅샷 출력"
    ),
    tr!(
        "stream sequenced UHP events",
        "transmitir eventos UHP secuenciados",
        "transmitir eventos UHP sequenciados",
        "diffuser les événements UHP séquencés",
        "sequenzierte UHP-Ereignisse streamen",
        "alirkan peristiwa UHP berurutan",
        "流式输出带序号的 UHP 事件",
        "連番付き UHP イベントを配信",
        "순서가 있는 UHP 이벤트 스트리밍"
    ),
    tr!(
        "forward one JSON request from stdin to the selected server",
        "reenviar una solicitud JSON de stdin al servidor seleccionado",
        "encaminhar uma solicitação JSON de stdin ao servidor selecionado",
        "transmettre une requête JSON de stdin au serveur sélectionné",
        "eine JSON-Anfrage von stdin an den ausgewählten Server weiterleiten",
        "teruskan satu permintaan JSON dari stdin ke server terpilih",
        "将 stdin 中的一条 JSON 请求转发到所选服务器",
        "stdin の JSON リクエスト1件を選択したサーバーへ転送",
        "stdin의 JSON 요청 하나를 선택한 서버로 전달"
    ),
    tr!(
        "list default and named server sessions",
        "listar sesiones predeterminadas y con nombre",
        "listar sessões padrão e nomeadas",
        "lister les sessions serveur par défaut et nommées",
        "Standard- und benannte Serversitzungen auflisten",
        "daftar sesi server bawaan dan bernama",
        "列出默认和命名服务器会话",
        "既定および名前付きサーバーセッションを一覧表示",
        "기본 및 이름 있는 서버 세션 나열"
    ),
    tr!(
        "start or attach to the named session",
        "iniciar o conectar a la sesión con nombre",
        "iniciar ou anexar à sessão nomeada",
        "démarrer ou rejoindre la session nommée",
        "benannte Sitzung starten oder verbinden",
        "mulai atau sambungkan ke sesi bernama",
        "启动或连接到命名会话",
        "名前付きセッションを起動または接続",
        "이름 있는 세션 시작 또는 연결"
    ),
    tr!(
        "stop only the named session and its panes",
        "detener solo la sesión con nombre y sus paneles",
        "parar apenas a sessão nomeada e seus painéis",
        "arrêter uniquement la session nommée et ses volets",
        "nur die benannte Sitzung und ihre Bereiche stoppen",
        "hentikan hanya sesi bernama dan panelnya",
        "仅停止命名会话及其窗格",
        "名前付きセッションとそのペインのみ停止",
        "이름 있는 세션과 그 패널만 중지"
    ),
    tr!(
        "delete a stopped named session",
        "eliminar una sesión con nombre detenida",
        "excluir uma sessão nomeada parada",
        "supprimer une session nommée arrêtée",
        "gestoppte benannte Sitzung löschen",
        "hapus sesi bernama yang berhenti",
        "删除已停止的命名会话",
        "停止済みの名前付きセッションを削除",
        "중지된 이름 있는 세션 삭제"
    ),
    tr!(
        "attach to a luvus session on <host> over plain ssh",
        "conectar a una sesión de luvus en <host> mediante ssh",
        "anexar a uma sessão luvus em <host> via ssh",
        "rejoindre une session luvus sur <host> via ssh",
        "über ssh mit einer luvus-Sitzung auf <host> verbinden",
        "sambungkan ke sesi luvus di <host> melalui ssh",
        "通过 ssh 连接到 <host> 上的 luvus 会话",
        "ssh で <host> の luvus セッションに接続",
        "일반 ssh로 <host>의 luvus 세션에 연결"
    ),
    tr!(
        "is the server running, and what version",
        "comprobar si el servidor funciona y su versión",
        "verificar se o servidor está ativo e sua versão",
        "indiquer si le serveur fonctionne et sa version",
        "prüfen, ob der Server läuft und welche Version",
        "periksa apakah server berjalan dan versinya",
        "检查服务器是否运行及其版本",
        "サーバーの稼働状態とバージョンを確認",
        "서버 실행 여부와 버전 확인"
    ),
    tr!(
        "start the background server if it isn't up",
        "iniciar el servidor en segundo plano si no está activo",
        "iniciar o servidor em segundo plano se não estiver ativo",
        "démarrer le serveur d'arrière-plan s'il est arrêté",
        "Hintergrundserver starten, falls er nicht läuft",
        "jalankan server latar jika belum aktif",
        "若后台服务器未运行则启动",
        "バックグラウンドサーバーが未起動なら開始",
        "백그라운드 서버가 꺼져 있으면 시작"
    ),
    tr!(
        "stop the server (and all panes)",
        "detener el servidor (y todos los paneles)",
        "parar o servidor (e todos os painéis)",
        "arrêter le serveur (et tous les volets)",
        "Server stoppen (und alle Bereiche)",
        "hentikan server (dan semua panel)",
        "停止服务器（及所有窗格）",
        "サーバーを停止（全ペインを含む）",
        "서버 중지 (모든 패널 포함)"
    ),
    tr!(
        "stop + start (load a newly-installed binary)",
        "detener e iniciar (cargar un binario recién instalado)",
        "parar e iniciar (carregar um binário recém-instalado)",
        "arrêter puis démarrer (charger un binaire nouvellement installé)",
        "stoppen und starten (neu installiertes Binary laden)",
        "hentikan lalu mulai (muat binary baru)",
        "停止并启动（加载新安装的二进制文件）",
        "停止して再開（新しくインストールしたバイナリを読み込む）",
        "중지 후 시작 (새로 설치된 바이너리 적용)"
    ),
    tr!(
        "fetch the latest agent-detection rules from luvus.dev",
        "obtener las reglas más recientes de detección desde luvus.dev",
        "buscar as regras mais recentes de detecção em luvus.dev",
        "récupérer les dernières règles de détection depuis luvus.dev",
        "neueste Agentenerkennungsregeln von luvus.dev abrufen",
        "ambil aturan deteksi agen terbaru dari luvus.dev",
        "从 luvus.dev 获取最新智能体检测规则",
        "luvus.dev から最新のエージェント検出ルールを取得",
        "luvus.dev에서 최신 에이전트 감지 규칙 가져오기"
    ),
    tr!(
        "(applies live if the server is up; else on next start)",
        "(se aplica en vivo si el servidor está activo; si no, al iniciar)",
        "(aplica ao vivo se o servidor estiver ativo; senão, na próxima inicialização)",
        "(appliqué en direct si le serveur tourne, sinon au prochain démarrage)",
        "(bei laufendem Server sofort, sonst beim nächsten Start)",
        "(langsung berlaku jika server aktif; jika tidak, saat mulai berikutnya)",
        "（服务器运行时实时应用，否则下次启动时应用）",
        "（サーバー稼働中は即時、停止中は次回起動時に適用）",
        "(서버가 실행 중이면 즉시 적용, 아니면 다음 시작 시 적용)"
    ),
    tr!(
        "add/remove luvus's session-resume hook (uninstall",
        "añadir/eliminar el hook de reanudación de luvus (uninstall",
        "adicionar/remover o hook de retomada do luvus (uninstall",
        "ajouter/supprimer le hook de reprise de luvus (uninstall",
        "Luvus-Hook zur Sitzungsfortsetzung hinzufügen/entfernen (uninstall",
        "tambah/hapus hook pelanjutan sesi luvus (uninstall",
        "添加/移除 luvus 会话恢复钩子（uninstall",
        "luvus のセッション再開フックを追加/削除（uninstall",
        "luvus의 세션 재개 훅 추가/제거 (제거 시"
    ),
    tr!(
        "removes only luvus's hook, never the agent)",
        "solo elimina el hook de luvus, nunca el agente)",
        "remove apenas o hook do luvus, nunca o agente)",
        "ne supprime que le hook de luvus, jamais l'agent)",
        "entfernt nur den Luvus-Hook, niemals den Agenten)",
        "hanya menghapus hook luvus, bukan agennya)",
        "仅移除 luvus 钩子，不会移除智能体）",
        "luvus のフックのみ削除し、エージェントは削除しない）",
        "luvus의 훅만 제거하며 에이전트는 건드리지 않음)"
    ),
];

/// Local CLI labels and diagnostics. These are kept out of `HELP` so short
/// labels such as `name` can never be mistaken for a help-row description.
static TEXT: &[Translation] = &[
    tr!(
        "Could not authorize UHP access.",
        "No se pudo autorizar el acceso UHP.",
        "Não foi possível autorizar o acesso UHP.",
        "Impossible d'autoriser l'accès UHP.",
        "Der UHP-Zugriff konnte nicht autorisiert werden.",
        "Akses UHP tidak dapat diotorisasi.",
        "无法授权 UHP 访问。",
        "UHP アクセスを承認できませんでした。",
        "UHP 액세스를 승인할 수 없습니다."
    ),
    tr!(
        "Could not start the private UHP access gateway.",
        "No se pudo iniciar la puerta de enlace privada de acceso UHP.",
        "Não foi possível iniciar o gateway privado de acesso UHP.",
        "Impossible de démarrer la passerelle privée d'accès UHP.",
        "Das private UHP-Zugriffs-Gateway konnte nicht gestartet werden.",
        "Gateway akses UHP privat tidak dapat dimulai.",
        "无法启动私有 UHP 访问网关。",
        "プライベート UHP アクセスゲートウェイを起動できませんでした。",
        "비공개 UHP 액세스 게이트웨이를 시작할 수 없습니다."
    ),
    tr!(
        "Could not create a secure pairing code.",
        "No se pudo crear un código de vinculación seguro.",
        "Não foi possível criar um código de pareamento seguro.",
        "Impossible de créer un code de jumelage sécurisé.",
        "Es konnte kein sicherer Kopplungscode erstellt werden.",
        "Kode pemasangan aman tidak dapat dibuat.",
        "无法创建安全配对码。",
        "安全なペアリングコードを作成できませんでした。",
        "안전한 페어링 코드를 만들 수 없습니다."
    ),
    tr!("name", "nombre", "nome", "nom", "Name", "nama", "名称", "名前",
        "이름"),
    tr!("status", "estado", "status", "état", "Status", "status", "状态", "状態",
        "상태"),
    tr!("session", "sesión", "sessão", "session", "Sitzung", "sesi", "会话", "セッション",
        "세션"),
    tr!("server", "servidor", "servidor", "serveur", "Server", "server", "服务器", "サーバー",
        "서버"),
    tr!("panes", "paneles", "painéis", "volets", "Bereiche", "panel", "窗格", "ペイン",
        "패널"),
    tr!("version", "versión", "versão", "version", "Version", "versi", "版本", "バージョン",
        "버전"),
    tr!("socket", "socket", "socket", "socket", "Socket", "socket", "套接字", "ソケット",
        "소켓"),
    tr!("detached", "desconectada", "desanexada", "détachée", "getrennt", "terlepas", "已分离", "デタッチ済み",
        "분리됨"),
    tr!("directory", "directorio", "diretório", "répertoire", "Verzeichnis", "direktori", "目录", "ディレクトリ",
        "디렉터리"),
    tr!("running", "en ejecución", "em execução", "en cours", "läuft", "berjalan", "运行中", "実行中",
        "실행 중"),
    tr!("started", "iniciado", "iniciado", "démarré", "gestartet", "dimulai", "已启动", "起動済み",
        "시작됨"),
    tr!("restarted", "reiniciado", "reiniciado", "redémarré", "neu gestartet", "dimulai ulang", "已重启", "再起動済み",
        "재시작됨"),
    tr!("stopped", "detenida", "parada", "arrêtée", "gestoppt", "berhenti", "已停止", "停止済み",
        "중지됨"),
    tr!("stopped session", "sesión detenida", "sessão parada", "session arrêtée", "Sitzung gestoppt", "sesi dihentikan", "已停止会话", "セッションを停止しました",
        "세션 중지됨"),
    tr!("deleted session", "sesión eliminada", "sessão excluída", "session supprimée", "Sitzung gelöscht", "sesi dihapus", "已删除会话", "セッションを削除しました",
        "세션 삭제됨"),
    tr!("unknown help topic", "tema de ayuda desconocido", "tópico de ajuda desconhecido", "sujet d'aide inconnu", "unbekanntes Hilfethema", "topik bantuan tidak dikenal", "未知帮助主题", "不明なヘルプトピック",
        "알 수 없는 도움말 항목"),
    tr!("Run `luvus --help` for the list.", "Ejecuta `luvus --help` para ver la lista.", "Execute `luvus --help` para ver a lista.", "Exécutez `luvus --help` pour voir la liste.", "Mit `luvus --help` wird die Liste angezeigt.", "Jalankan `luvus --help` untuk melihat daftar.", "运行 `luvus --help` 查看列表。", "一覧は `luvus --help` で確認できます。",
        "목록을 보려면 `luvus --help`를 실행하세요."),
    tr!("Check whether the selected server responds.", "Comprobar si responde el servidor seleccionado.", "Verificar se o servidor selecionado responde.", "Vérifier si le serveur sélectionné répond.", "Prüfen, ob der ausgewählte Server antwortet.", "Periksa apakah server terpilih merespons.", "检查所选服务器是否响应。", "選択したサーバーが応答するか確認します。",
        "선택한 서버가 응답하는지 확인합니다."),
    tr!("Check optional external tools used by Luvus.", "Comprobar herramientas externas opcionales usadas por Luvus.", "Verificar ferramentas externas opcionais usadas pelo Luvus.", "Vérifier les outils externes facultatifs utilisés par Luvus.", "Von Luvus verwendete optionale externe Werkzeuge prüfen.", "Periksa alat eksternal opsional yang digunakan Luvus.", "检查 Luvus 使用的可选外部工具。", "Luvus が使用する任意の外部ツールを確認します。",
        "Luvus가 사용하는 선택적 외부 도구를 확인합니다."),
    tr!("Check for a newer release and install it through the detected safe update channel.", "Buscar una versión nueva e instalarla mediante el canal seguro detectado.", "Verificar uma nova versão e instalá-la pelo canal seguro detectado.", "Rechercher une nouvelle version et l'installer via le canal sûr détecté.", "Nach neuer Version suchen und über den erkannten sicheren Kanal installieren.", "Periksa rilis baru dan pasang lewat kanal aman yang terdeteksi.", "检查新版本并通过检测到的安全更新渠道安装。", "新しいリリースを確認し、検出した安全な更新経路でインストールします。",
        "새 릴리스를 확인하고 감지된 안전한 업데이트 채널을 통해 설치합니다."),
    tr!("Checking for Luvus updates...", "Buscando actualizaciones de Luvus...", "Verificando atualizações do Luvus...", "Recherche des mises à jour de Luvus...", "Luvus-Aktualisierungen werden gesucht...", "Memeriksa pembaruan Luvus...", "正在检查 Luvus 更新...", "Luvus の更新を確認しています...",
        "Luvus 업데이트를 확인하는 중..."),
    tr!("is already up to date.", "ya está actualizado.", "já está atualizado.", "est déjà à jour.", "ist bereits aktuell.", "sudah terbaru.", "已是最新版本。", "はすでに最新です。",
        "이미 최신 상태입니다."),
    tr!("Luvus {latest} is available (current: {current}).", "Luvus {latest} está disponible (actual: {current}).", "Luvus {latest} está disponível (atual: {current}).", "Luvus {latest} est disponible (version actuelle : {current}).", "Luvus {latest} ist verfügbar (aktuell: {current}).", "Luvus {latest} tersedia (saat ini: {current}).", "Luvus {latest} 可用（当前版本：{current}）。", "Luvus {latest} が利用できます（現在のバージョン：{current}）。",
        "Luvus {latest} 버전을 사용할 수 있습니다 (현재: {current})."),
    tr!("Updated Luvus", "Luvus actualizado", "Luvus atualizado", "Luvus mis à jour", "Luvus aktualisiert", "Luvus diperbarui", "Luvus 已更新", "Luvus を更新しました",
        "Luvus 업데이트됨"),
    tr!("Run `luvus server restart` when you are ready to load the new server binary.", "Ejecuta `luvus server restart` cuando quieras cargar el nuevo binario del servidor.", "Execute `luvus server restart` quando quiser carregar o novo binário do servidor.", "Exécutez `luvus server restart` lorsque vous souhaitez charger le nouveau binaire serveur.", "Führe `luvus server restart` aus, wenn das neue Server-Binary geladen werden soll.", "Jalankan `luvus server restart` saat siap memuat binary server baru.", "准备加载新的服务器二进制文件时，请运行 `luvus server restart`。", "新しいサーバーバイナリを読み込む準備ができたら `luvus server restart` を実行してください。",
        "새 서버 바이너리를 적용할 준비가 되면 `luvus server restart`를 실행하세요."),
    tr!("could not check", "no se pudo comprobar", "não foi possível verificar", "impossible de vérifier", "konnte nicht prüfen", "tidak dapat memeriksa", "无法检查", "確認できませんでした",
        "확인할 수 없음"),
    tr!("check your connection and try again", "comprueba tu conexión e inténtalo de nuevo", "verifique sua conexão e tente novamente", "vérifiez votre connexion et réessayez", "Verbindung prüfen und erneut versuchen", "periksa koneksi dan coba lagi", "请检查网络连接后重试", "接続を確認して再試行してください",
        "연결을 확인하고 다시 시도하세요"),
    tr!("the multiplexer (panes · tabs · agents) needs no external tools", "el multiplexor (paneles · pestañas · agentes) no necesita herramientas externas", "o multiplexador (painéis · abas · agentes) não precisa de ferramentas externas", "le multiplexeur (volets · onglets · agents) ne nécessite aucun outil externe", "der Multiplexer (Bereiche · Tabs · Agenten) benötigt keine externen Werkzeuge", "multiplexer (panel · tab · agen) tidak memerlukan alat eksternal", "多路复用器（窗格 · 标签页 · 智能体）无需外部工具", "マルチプレクサー（ペイン · タブ · エージェント）に外部ツールは不要です",
        "멀티플렉서(패널 · 탭 · 에이전트)는 외부 도구가 필요하지 않습니다"),
    tr!("GitHub PRs & issues", "PR e incidencias de GitHub", "PRs e issues do GitHub", "PR et issues GitHub", "GitHub-PRs und Issues", "PR dan issue GitHub", "GitHub PR 和议题", "GitHub PR と Issue",
        "GitHub PR 및 이슈"),
    tr!("preinstalled on macOS/Linux", "preinstalado en macOS/Linux", "pré-instalado no macOS/Linux", "préinstallé sur macOS/Linux", "auf macOS/Linux vorinstalliert", "sudah terpasang di macOS/Linux", "macOS/Linux 已预装", "macOS/Linux にプリインストール済み",
        "macOS/Linux에 기본 설치됨"),
    tr!("needed for", "necesario para", "necessário para", "requis pour", "benötigt für", "diperlukan untuk", "需要用于", "必要：",
        "다음에 필요"),
    tr!("optional -", "opcional -", "opcional -", "facultatif -", "optional -", "opsional -", "可选 -", "任意 -",
        "선택 -"),
    tr!("not found", "no encontrado", "não encontrado", "introuvable", "nicht gefunden", "tidak ditemukan", "未找到", "見つかりません",
        "찾을 수 없음"),
    tr!("Luvus was not found on the remote host {host}.", "No se encontró Luvus en el host remoto {host}.", "O Luvus não foi encontrado no host remoto {host}.", "Luvus est introuvable sur l’hôte distant {host}.", "Luvus wurde auf dem Remotehost {host} nicht gefunden.", "Luvus tidak ditemukan di host jarak jauh {host}.", "在远程主机 {host} 上未找到 Luvus。", "リモートホスト {host} に Luvus が見つかりませんでした。",
        "원격 호스트 {host}에서 Luvus를 찾을 수 없습니다."),
    tr!("Install Luvus there or place it in PATH or a standard user installation directory.", "Instala Luvus allí o colócalo en PATH o en un directorio de instalación de usuario estándar.", "Instale o Luvus nesse host ou coloque-o no PATH ou em um diretório padrão de instalação do usuário.", "Installez Luvus sur cet hôte ou placez-le dans PATH ou dans un répertoire d’installation utilisateur standard.", "Installieren Sie Luvus dort oder legen Sie es im PATH oder in einem üblichen Benutzer-Installationsverzeichnis ab.", "Instal Luvus di host tersebut atau letakkan di PATH maupun direktori instalasi pengguna standar.", "请在该主机上安装 Luvus，或将其放入 PATH 或标准用户安装目录。", "そのホストに Luvus をインストールするか、PATH または標準のユーザーインストールディレクトリに配置してください。",
        "해당 호스트에 Luvus를 설치하거나 PATH 또는 표준 사용자 설치 디렉터리에 배치하세요."),
    tr!("directory writable", "directorio escribible", "diretório gravável", "répertoire accessible en écriture", "Verzeichnis beschreibbar", "direktori dapat ditulis", "目录可写", "ディレクトリに書き込み可能",
        "디렉터리 쓰기 가능"),
    tr!("not writable", "no escribible", "não gravável", "non accessible en écriture", "nicht beschreibbar", "tidak dapat ditulis", "不可写", "書き込み不可",
        "쓰기 불가"),
    tr!("run `luvus doctor` outside a luvus pane to test your terminal", "ejecuta `luvus doctor` fuera de un panel de luvus para probar tu terminal", "execute `luvus doctor` fora de um painel luvus para testar seu terminal", "exécutez `luvus doctor` hors d'un volet luvus pour tester votre terminal", "`luvus doctor` außerhalb eines Luvus-Bereichs ausführen, um das Terminal zu testen", "jalankan `luvus doctor` di luar panel luvus untuk menguji terminal", "请在 luvus 窗格外运行 `luvus doctor` 以测试终端", "端末を確認するには luvus ペイン外で `luvus doctor` を実行してください",
        "터미널을 테스트하려면 luvus 패널 밖에서 `luvus doctor`를 실행하세요"),
    tr!("Shift+Enter works (terminal reports modified keys)", "Shift+Enter funciona (el terminal informa teclas modificadas)", "Shift+Enter funciona (o terminal informa teclas modificadas)", "Shift+Entrée fonctionne (le terminal signale les touches modifiées)", "Shift+Enter funktioniert (Terminal meldet modifizierte Tasten)", "Shift+Enter berfungsi (terminal melaporkan tombol bermodifier)", "Shift+Enter 可用（终端报告修饰键）", "Shift+Enter は利用できます（端末が修飾キーを通知）",
        "Shift+Enter 작동함 (터미널이 수식 키 입력을 보고함)"),
    tr!("Shift+Enter isn't distinguishable here · optional", "Shift+Enter no se distingue aquí · opcional", "Shift+Enter não é distinguível aqui · opcional", "Shift+Entrée n'est pas distinguable ici · facultatif", "Shift+Enter ist hier nicht unterscheidbar · optional", "Shift+Enter tidak dapat dibedakan di sini · opsional", "此处无法区分 Shift+Enter · 可选", "ここでは Shift+Enter を区別できません · 任意",
        "여기서는 Shift+Enter를 구분할 수 없음 · 선택 사항"),
    tr!("WSL in Windows Terminal detected; all other features still work", "se detectó WSL en Windows Terminal; las demás funciones siguen disponibles", "WSL no Windows Terminal detectado; os demais recursos continuam funcionando", "WSL dans Windows Terminal détecté ; les autres fonctions restent disponibles", "WSL in Windows Terminal erkannt; alle anderen Funktionen arbeiten weiter", "WSL di Windows Terminal terdeteksi; fitur lain tetap berfungsi", "检测到 Windows Terminal 中的 WSL；其他功能仍可用", "Windows Terminal 上の WSL を検出。他の機能は引き続き利用できます",
        "Windows Terminal의 WSL 감지됨; 다른 모든 기능은 정상 작동"),
    tr!("update Windows Terminal to 1.25+, or bind Shift+Enter to ESC CR", "actualiza Windows Terminal a 1.25+ o vincula Shift+Enter a ESC CR", "atualize o Windows Terminal para 1.25+ ou vincule Shift+Enter a ESC CR", "mettez Windows Terminal à jour vers 1.25+ ou liez Shift+Entrée à ESC CR", "Windows Terminal auf 1.25+ aktualisieren oder Shift+Enter an ESC CR binden", "perbarui Windows Terminal ke 1.25+ atau ikat Shift+Enter ke ESC CR", "将 Windows Terminal 更新到 1.25+，或将 Shift+Enter 绑定为 ESC CR", "Windows Terminal を 1.25+ に更新するか Shift+Enter を ESC CR に割り当ててください",
        "Windows Terminal을 1.25 이상으로 업데이트하거나 Shift+Enter를 ESC CR에 바인딩하세요"),
    tr!("WSL detected; all other features still work", "se detectó WSL; las demás funciones siguen disponibles", "WSL detectado; os demais recursos continuam funcionando", "WSL détecté ; les autres fonctions restent disponibles", "WSL erkannt; alle anderen Funktionen arbeiten weiter", "WSL terdeteksi; fitur lain tetap berfungsi", "检测到 WSL；其他功能仍可用", "WSL を検出。他の機能は引き続き利用できます",
        "WSL 감지됨; 다른 모든 기능은 정상 작동"),
    tr!("use Windows Terminal 1.25+ or bind Shift+Enter to ESC CR", "usa Windows Terminal 1.25+ o vincula Shift+Enter a ESC CR", "use Windows Terminal 1.25+ ou vincule Shift+Enter a ESC CR", "utilisez Windows Terminal 1.25+ ou liez Shift+Entrée à ESC CR", "Windows Terminal 1.25+ verwenden oder Shift+Enter an ESC CR binden", "gunakan Windows Terminal 1.25+ atau ikat Shift+Enter ke ESC CR", "使用 Windows Terminal 1.25+，或将 Shift+Enter 绑定为 ESC CR", "Windows Terminal 1.25+ を使うか Shift+Enter を ESC CR に割り当ててください",
        "Windows Terminal 1.25 이상을 사용하거나 Shift+Enter를 ESC CR에 바인딩하세요"),
    tr!("Luvus still works; only the modified-Enter shortcut is affected", "Luvus sigue funcionando; solo afecta al atajo Enter modificado", "Luvus continua funcionando; apenas o atalho Enter modificado é afetado", "Luvus fonctionne toujours ; seul le raccourci Entrée modifiée est affecté", "Luvus funktioniert weiter; nur der modifizierte Enter-Kurzbefehl ist betroffen", "Luvus tetap berfungsi; hanya pintasan Enter bermodifier yang terpengaruh", "Luvus 仍可正常工作；仅修饰 Enter 快捷键受影响", "Luvus は動作します。修飾 Enter のショートカットだけが影響を受けます",
        "Luvus는 계속 작동하며 수식 키가 포함된 Enter 단축키만 영향을 받습니다"),
    tr!("use Alt/Option+Enter or a terminal with the keyboard protocol", "usa Alt/Option+Enter o un terminal con el protocolo de teclado", "use Alt/Option+Enter ou um terminal com o protocolo de teclado", "utilisez Alt/Option+Entrée ou un terminal avec le protocole clavier", "Alt/Option+Enter oder ein Terminal mit Tastaturprotokoll verwenden", "gunakan Alt/Option+Enter atau terminal dengan protokol keyboard", "请使用 Alt/Option+Enter 或支持键盘协议的终端", "Alt/Option+Enter またはキーボードプロトコル対応端末を使用してください",
        "Alt/Option+Enter를 사용하거나 키보드 프로토콜을 지원하는 터미널을 사용하세요"),
    tr!("Tip: install `git` to use the git tab & worktrees. Everything else works now.", "Consejo: instala `git` para usar la pestaña Git y los worktrees. Todo lo demás ya funciona.", "Dica: instale `git` para usar a aba Git e worktrees. Todo o restante já funciona.", "Conseil : installez `git` pour utiliser l'onglet Git et les worktrees. Tout le reste fonctionne.", "Tipp: `git` für Git-Tab und Worktrees installieren. Alles andere funktioniert bereits.", "Tip: pasang `git` untuk memakai tab Git dan worktree. Fitur lain sudah berfungsi.", "提示：安装 `git` 以使用 Git 标签页和工作树。其他功能均可正常使用。", "ヒント：Git タブとワークツリーには `git` をインストールしてください。他はすべて利用できます。",
        "팁: git 탭과 worktree를 사용하려면 `git`을 설치하세요. 나머지 기능은 지금 바로 작동합니다."),
    tr!("All set — you're good to go. ✓", "Todo listo. ✓", "Tudo pronto. ✓", "Tout est prêt. ✓", "Alles bereit. ✓", "Semua siap. ✓", "一切就绪。✓", "準備完了です。✓",
        "모두 준비됨 — 바로 사용할 수 있습니다. ✓"),
    tr!("Installed Luvus integration for {agent}.", "Integración de Luvus instalada para {agent}.", "Integração do Luvus instalada para {agent}.", "Intégration Luvus installée pour {agent}.", "Luvus-Integration für {agent} installiert.", "Integrasi Luvus untuk {agent} telah dipasang.", "已为 {agent} 安装 Luvus 集成。", "{agent} 用の Luvus 連携をインストールしました。",
        "{agent}용 Luvus 연동을 설치했습니다."),
    tr!("Removed Luvus integration for {agent}. The agent itself was not changed.", "Integración de Luvus eliminada para {agent}. El agente no se ha modificado.", "Integração do Luvus removida para {agent}. O agente não foi alterado.", "Intégration Luvus supprimée pour {agent}. L'agent n'a pas été modifié.", "Luvus-Integration für {agent} entfernt. Der Agent selbst wurde nicht geändert.", "Integrasi Luvus untuk {agent} telah dihapus. Agen tidak diubah.", "已移除 {agent} 的 Luvus 集成。智能体本身未被修改。", "{agent} 用の Luvus 連携を削除しました。エージェント自体は変更されていません。",
        "{agent}용 Luvus 연동을 제거했습니다. 에이전트 자체는 변경되지 않았습니다."),
    tr!("Unsupported agent: {agent} (supported: {supported})", "Agente no compatible: {agent} (compatibles: {supported})", "Agente não suportado: {agent} (suportados: {supported})", "Agent non pris en charge : {agent} (pris en charge : {supported})", "Nicht unterstützter Agent: {agent} (unterstützt: {supported})", "Agen tidak didukung: {agent} (didukung: {supported})", "不支持的智能体：{agent}（支持：{supported}）", "未対応のエージェント：{agent}（対応：{supported}）",
        "지원하지 않는 에이전트: {agent} (지원 목록: {supported})"),
    tr!("unknown command. Try `luvus --help`.", "comando desconocido. Ejecuta `luvus --help`.", "comando desconhecido. Execute `luvus --help`.", "commande inconnue. Exécutez `luvus --help`.", "unbekannter Befehl. Führe `luvus --help` aus.", "perintah tidak dikenal. Jalankan `luvus --help`.", "未知命令。请运行 `luvus --help`。", "不明なコマンドです。`luvus --help` を実行してください。",
        "알 수 없는 명령어입니다. `luvus --help`를 시도하세요."),
    tr!("`luvus skill update` was removed; update Luvus, then run `luvus skill enable` to install its version-matched skill", "Se eliminó `luvus skill update`; actualiza Luvus y ejecuta `luvus skill enable` para instalar la skill de la versión correspondiente", "`luvus skill update` foi removido; atualize o Luvus e execute `luvus skill enable` para instalar a skill da versão correspondente", "`luvus skill update` a été supprimé ; mettez Luvus à jour, puis exécutez `luvus skill enable` pour installer la skill correspondant à cette version", "`luvus skill update` wurde entfernt; aktualisiere Luvus und führe dann `luvus skill enable` aus, um den zur Version passenden Skill zu installieren", "`luvus skill update` telah dihapus; perbarui Luvus, lalu jalankan `luvus skill enable` untuk memasang skill yang sesuai dengan versinya", "`luvus skill update` 已移除。请更新 Luvus，然后运行 `luvus skill enable` 安装与版本匹配的技能", "`luvus skill update` は削除されました。Luvus を更新してから `luvus skill enable` を実行し、バージョンに対応するスキルをインストールしてください",
        "`luvus skill update`는 제거되었습니다; Luvus를 업데이트한 후 `luvus skill enable`을 실행해 버전이 일치하는 스킬을 설치하세요"),
    tr!("`luvus skill {command}` was removed; use `luvus skill {replacement}`", "Se eliminó `luvus skill {command}`; usa `luvus skill {replacement}`", "`luvus skill {command}` foi removido; use `luvus skill {replacement}`", "`luvus skill {command}` a été supprimé ; utilisez `luvus skill {replacement}`", "`luvus skill {command}` wurde entfernt; verwende `luvus skill {replacement}`", "`luvus skill {command}` telah dihapus; gunakan `luvus skill {replacement}`", "`luvus skill {command}` 已移除。请使用 `luvus skill {replacement}`", "`luvus skill {command}` は削除されました。`luvus skill {replacement}` を使用してください",
        "`luvus skill {command}`는 제거되었습니다; `luvus skill {replacement}`를 사용하세요"),
    tr!("unknown skill command `{command}`; expected enable, status, disable, or show", "comando de skill desconocido `{command}`; se esperaba enable, status, disable o show", "comando de skill desconhecido `{command}`; esperado enable, status, disable ou show", "commande de skill inconnue `{command}` ; attendu : enable, status, disable ou show", "unbekannter Skill-Befehl `{command}`; erwartet wurde enable, status, disable oder show", "perintah skill tidak dikenal `{command}`; gunakan enable, status, disable, atau show", "未知技能命令 `{command}`。应为 enable、status、disable 或 show", "不明なスキルコマンド `{command}`。enable、status、disable、show のいずれかを指定してください",
        "알 수 없는 스킬 명령어 `{command}`; enable, status, disable, show 중 하나여야 합니다"),
    tr!("unknown server command", "comando de servidor desconocido", "comando de servidor desconhecido", "commande serveur inconnue", "unbekannter Serverbefehl", "perintah server tidak dikenal", "未知服务器命令", "不明なサーバーコマンド",
        "알 수 없는 서버 명령어"),
    tr!("server already running", "el servidor ya está en ejecución", "o servidor já está em execução", "le serveur est déjà en cours", "Server läuft bereits", "server sudah berjalan", "服务器已在运行", "サーバーはすでに実行中です",
        "서버가 이미 실행 중입니다"),
    tr!("server started", "servidor iniciado", "servidor iniciado", "serveur démarré", "Server gestartet", "server dimulai", "服务器已启动", "サーバーを起動しました",
        "서버 시작됨"),
    tr!("server stopped", "servidor detenido", "servidor parado", "serveur arrêté", "Server gestoppt", "server dihentikan", "服务器已停止", "サーバーを停止しました",
        "서버 중지됨"),
    tr!("no luvus server running", "no hay ningún servidor luvus en ejecución", "nenhum servidor luvus em execução", "aucun serveur luvus en cours", "kein Luvus-Server läuft", "tidak ada server luvus berjalan", "没有正在运行的 luvus 服务器", "実行中の luvus サーバーはありません",
        "실행 중인 luvus 서버 없음"),
    tr!("server restarted", "servidor reiniciado", "servidor reiniciado", "serveur redémarré", "Server neu gestartet", "server dimulai ulang", "服务器已重启", "サーバーを再起動しました",
        "서버 재시작됨"),
    tr!("not running", "no está en ejecución", "não está em execução", "arrêté", "läuft nicht", "tidak berjalan", "未运行", "停止中",
        "실행 중이 아님"),
    tr!("note: this binary is", "nota: este binario es", "nota: este binário é", "note : ce binaire est", "Hinweis: Dieses Binary ist", "catatan: binary ini", "注意：当前二进制版本为", "注：このバイナリは",
        "참고: 이 바이너리는"),
    tr!("run `luvus server restart` to load it", "ejecuta `luvus server restart` para cargarlo", "execute `luvus server restart` para carregá-lo", "exécutez `luvus server restart` pour le charger", "mit `luvus server restart` laden", "jalankan `luvus server restart` untuk memuatnya", "运行 `luvus server restart` 以加载它", "読み込むには `luvus server restart` を実行してください",
        "적용하려면 `luvus server restart`를 실행하세요"),
    tr!("server is running but did not answer", "el servidor está en ejecución pero no respondió", "o servidor está em execução mas não respondeu", "le serveur fonctionne mais n'a pas répondu", "Server läuft, hat aber nicht geantwortet", "server berjalan tetapi tidak merespons", "服务器正在运行但未响应", "サーバーは実行中ですが応答しませんでした",
        "서버가 실행 중이지만 응답하지 않았습니다"),
    tr!("built-in", "integrado", "integrado", "intégré", "integriert", "bawaan", "内置", "組み込み",
        "내장"),
    tr!("local", "local", "local", "local", "lokal", "lokal", "本地", "ローカル",
        "로컬"),
    tr!("virtual", "virtual", "virtual", "virtuel", "virtuell", "virtual", "虚拟", "仮想",
        "가상"),
    tr!("warning", "advertencia", "aviso", "avertissement", "Warnung", "peringatan", "警告", "警告",
        "경고"),
    tr!("invalid", "no válido", "inválido", "invalide", "ungültig", "tidak valid", "无效", "無効",
        "유효하지 않음"),
    tr!("created", "creado", "criado", "créé", "erstellt", "dibuat", "已创建", "作成しました",
        "생성됨"),
    tr!("valid theme", "tema válido", "tema válido", "thème valide", "gültiges Theme", "tema valid", "有效主题", "有効なテーマ",
        "유효한 테마"),
    tr!("installed", "instalado", "instalado", "installé", "installiert", "terpasang", "已安装", "インストール済み",
        "설치됨"),
    tr!("from", "desde", "de", "depuis", "von", "dari", "来源", "取得元",
        "출처"),
    tr!("to", "en", "em", "vers", "nach", "ke", "到", "保存先",
        "대상"),
    tr!("and reloaded the selected server", "y se recargó el servidor seleccionado", "e o servidor selecionado foi recarregado", "et le serveur sélectionné a été rechargé", "und ausgewählten Server neu geladen", "dan server terpilih dimuat ulang", "并已重新加载所选服务器", "選択したサーバーを再読み込みしました",
        "그리고 선택한 서버를 다시 불러왔습니다"),
    tr!("start or reload Luvus to use it", "inicia o recarga Luvus para usarlo", "inicie ou recarregue o Luvus para usá-lo", "démarrez ou rechargez Luvus pour l'utiliser", "Luvus starten oder neu laden, um es zu verwenden", "mulai atau muat ulang Luvus untuk memakainya", "启动或重新加载 Luvus 以使用它", "使用するには Luvus を起動または再読み込みしてください",
        "사용하려면 Luvus를 시작하거나 다시 불러오세요"),
    tr!("using theme", "usando el tema", "usando o tema", "thème utilisé", "Theme wird verwendet", "menggunakan tema", "正在使用主题", "使用中のテーマ",
        "사용 중인 테마"),
    tr!("applies when Luvus starts", "se aplica cuando Luvus se inicia", "aplica quando o Luvus iniciar", "s'applique au démarrage de Luvus", "wird beim Start von Luvus angewendet", "berlaku saat Luvus dimulai", "将在 Luvus 启动时应用", "Luvus 起動時に適用されます",
        "Luvus 시작 시 적용됨"),
    tr!("could not save the theme selection", "no se pudo guardar la selección del tema", "não foi possível salvar a seleção do tema", "impossible d’enregistrer la sélection du thème", "Theme-Auswahl konnte nicht gespeichert werden", "pilihan tema tidak dapat disimpan", "无法保存主题选择", "テーマの選択を保存できませんでした",
        "테마 선택을 저장할 수 없습니다"),
    tr!("uninstalled", "desinstalado", "desinstalado", "désinstallé", "deinstalliert", "dihapus", "已卸载", "アンインストール済み",
        "제거됨"),
    tr!("reloaded", "recargados", "recarregados", "rechargés", "neu geladen", "dimuat ulang", "已重新加载", "再読み込みしました",
        "다시 불러옴"),
    tr!("themes", "temas", "temas", "thèmes", "Themes", "tema", "个主题", "テーマ",
        "테마"),
    tr!("validated", "validados", "validados", "validés", "geprüft", "tervalidasi", "已验证", "検証済み",
        "검증됨"),
    tr!("start Luvus to load them", "inicia Luvus para cargarlos", "inicie o Luvus para carregá-los", "démarrez Luvus pour les charger", "Luvus starten, um sie zu laden", "mulai Luvus untuk memuatnya", "启动 Luvus 以加载它们", "読み込むには Luvus を起動してください",
        "적용하려면 Luvus를 시작하세요"),
    tr!("theme is not installed", "el tema no está instalado", "o tema não está instalado", "le thème n'est pas installé", "Theme ist nicht installiert", "tema belum terpasang", "主题未安装", "テーマがインストールされていません",
        "테마가 설치되지 않음"),
    tr!("start luvus to use it", "inicia luvus para usarlo", "inicie o luvus para usá-lo", "démarrez luvus pour l'utiliser", "luvus starten, um es zu verwenden", "mulai luvus untuk memakainya", "启动 luvus 以使用它", "使用するには luvus を起動してください",
        "사용하려면 luvus를 시작하세요"),
    tr!("No modules found in the `luvus-module` topic yet.", "Aún no hay módulos en el tema `luvus-module`.", "Ainda não há módulos no tópico `luvus-module`.", "Aucun module trouvé dans le sujet `luvus-module` pour le moment.", "Noch keine Module im Topic `luvus-module` gefunden.", "Belum ada modul di topik `luvus-module`.", "`luvus-module` 主题中尚未找到模块。", "`luvus-module` トピックにはまだモジュールがありません。",
        "`luvus-module` 토픽에서 아직 모듈을 찾지 못했습니다."),
    tr!("Publish one by tagging a public repo with the `luvus-module` topic.", "Publica uno etiquetando un repositorio público con el tema `luvus-module`.", "Publique um marcando um repositório público com o tópico `luvus-module`.", "Publiez-en un en ajoutant le sujet `luvus-module` à un dépôt public.", "Ein öffentliches Repository mit dem Topic `luvus-module` veröffentlichen.", "Terbitkan dengan memberi topik `luvus-module` pada repo publik.", "为公共仓库添加 `luvus-module` 主题即可发布模块。", "公開リポジトリに `luvus-module` トピックを付けて公開できます。",
        "공개 저장소에 `luvus-module` 토픽을 태그하여 게시하세요."),
    tr!("results. Install with:", "resultados. Instala con:", "resultados. Instale com:", "résultats. Installez avec :", "Ergebnisse. Installieren mit:", "hasil. Pasang dengan:", "个结果。安装命令：", "件。インストール：",
        "결과. 다음 명령으로 설치:"),
    tr!("skipping suspicious manifest name", "se omite un nombre de manifiesto sospechoso", "ignorando nome de manifesto suspeito", "nom de manifeste suspect ignoré", "verdächtiger Manifestname wird übersprungen", "melewati nama manifest mencurigakan", "跳过可疑清单名称", "不審なマニフェスト名をスキップ",
        "의심스러운 매니페스트 이름 건너뜀"),
    tr!("skipping", "se omite", "ignorando", "ignoré", "übersprungen", "melewati", "跳过", "スキップ",
        "건너뜀"),
    tr!("not a valid detection manifest", "no es un manifiesto de detección válido", "não é um manifesto de detecção válido", "n'est pas un manifeste de détection valide", "kein gültiges Erkennungsmanifest", "bukan manifest deteksi yang valid", "不是有效的检测清单", "有効な検出マニフェストではありません",
        "유효한 감지 매니페스트가 아님"),
    tr!("updated", "actualizados", "atualizados", "mis à jour", "aktualisiert", "diperbarui", "已更新", "更新しました",
        "업데이트됨"),
    tr!("detection manifest(s)", "manifiestos de detección", "manifestos de detecção", "manifestes de détection", "Erkennungsmanifeste", "manifest deteksi", "个检测清单", "件の検出マニフェスト",
        "감지 매니페스트"),
    tr!("skipped", "omitidos", "ignorados", "ignorés", "übersprungen", "dilewati", "已跳过", "スキップ",
        "건너뜀"),
    tr!("reloaded into the running server", "recargado en el servidor en ejecución", "recarregado no servidor em execução", "rechargé dans le serveur en cours", "in den laufenden Server neu geladen", "dimuat ulang ke server berjalan", "已重新加载到运行中的服务器", "実行中のサーバーへ再読み込み",
        "실행 중인 서버에 다시 불러옴"),
    tr!("rules active", "reglas activas", "regras ativas", "règles actives", "aktive Regeln", "aturan aktif", "条规则生效", "件のルールが有効",
        "규칙 활성"),
    tr!("no restart needed", "no se necesita reiniciar", "não é necessário reiniciar", "aucun redémarrage requis", "kein Neustart erforderlich", "tidak perlu mulai ulang", "无需重启", "再起動は不要",
        "재시작 불필요"),
    tr!("no server running - the update loads on next start", "no hay servidor en ejecución; la actualización se carga al iniciar", "nenhum servidor em execução; a atualização carrega na próxima inicialização", "aucun serveur en cours ; la mise à jour sera chargée au prochain démarrage", "kein Server läuft; Aktualisierung wird beim nächsten Start geladen", "tidak ada server berjalan; pembaruan dimuat saat mulai berikutnya", "没有服务器运行；更新将在下次启动时加载", "サーバーは停止中です。更新は次回起動時に読み込まれます",
        "실행 중인 서버 없음 - 다음 시작 시 업데이트 적용"),
    tr!("bundled", "incluido", "incluído", "intégré", "gebündelt", "bawaan", "内置", "同梱",
        "번들됨"),
    tr!("available", "disponible", "disponível", "disponible", "verfügbar", "tersedia", "可用", "利用可能",
        "사용 가능"),
    tr!("installations", "instalaciones", "instalações", "installations", "Installationen", "instalasi", "安装", "インストール",
        "설치"),
    tr!("attention", "requiere atención", "requer atenção", "attention requise", "Aufmerksamkeit nötig", "perlu perhatian", "需要处理", "要確認",
        "주의"),
    tr!("enabled", "activado", "ativado", "activé", "aktiviert", "diaktifkan", "已启用", "有効",
        "활성화됨"),
    tr!("disabled", "desactivado", "desativado", "désactivé", "deaktiviert", "dinonaktifkan", "已禁用", "無効",
        "비활성화됨"),
    tr!("current", "actual", "atual", "à jour", "aktuell", "terkini", "最新", "最新",
        "현재"),
    tr!("outdated", "desactualizado", "desatualizado", "obsolète", "veraltet", "kedaluwarsa", "已过期", "古い",
        "오래됨"),
    tr!("missing", "ausente", "ausente", "manquant", "fehlt", "hilang", "缺失", "不足",
        "없음"),
    tr!("modified", "modificado", "modificado", "modifié", "geändert", "diubah", "已修改", "変更済み",
        "수정됨"),
    tr!("external-current", "externo y actual", "externo e atual", "externe et à jour", "extern und aktuell", "eksternal dan terkini", "外部且最新", "外部・最新",
        "외부-최신"),
    tr!("external", "externo", "externo", "externe", "extern", "eksternal", "外部", "外部",
        "외부"),
    tr!("not-installed", "no instalado", "não instalado", "non installé", "nicht installiert", "belum terpasang", "未安装", "未インストール",
        "설치 안 됨"),
    tr!("not-detected", "no detectado", "não detectado", "non détecté", "nicht erkannt", "tidak terdeteksi", "未检测到", "未検出",
        "감지 안 됨"),
    tr!("refreshed", "actualizado", "atualizado", "actualisé", "aktualisiert", "disegarkan", "已刷新", "更新済み",
        "새로고침됨"),
    tr!("repaired", "reparado", "reparado", "réparé", "repariert", "diperbaiki", "已修复", "修復済み",
        "복구됨"),
    tr!("external-preserved", "externo conservado", "externo preservado", "externe conservée", "extern beibehalten", "eksternal dipertahankan", "已保留外部副本", "外部コピーを保持",
        "외부-보존됨"),
    tr!("modified-preserved", "modificado y conservado", "modificado e preservado", "modifié et conservé", "geändert und beibehalten", "perubahan dipertahankan", "已保留修改", "変更を保持",
        "수정-보존됨"),
    tr!("already-disabled", "ya desactivado", "já desativado", "déjà désactivé", "bereits deaktiviert", "sudah dinonaktifkan", "已禁用", "無効化済み",
        "이미 비활성화됨"),
    tr!("agent-specific skill management was removed", "se eliminó la gestión de skills por agente", "o gerenciamento de skills por agente foi removido", "la gestion des skills par agent a été supprimée", "die agentenspezifische Skill-Verwaltung wurde entfernt", "pengelolaan skill per agen telah dihapus", "已移除按智能体管理技能的功能", "エージェント別のスキル管理は削除されました",
        "에이전트별 스킬 관리 기능은 제거되었습니다"),
    tr!("accepts no arguments", "no acepta argumentos", "não aceita argumentos", "n'accepte aucun argument", "akzeptiert keine Argumente", "tidak menerima argumen", "不接受参数", "引数は指定できません",
        "인자를 받지 않습니다"),
    tr!("unexpected", "inesperado", "inesperado", "inattendu", "unerwartet", "tidak diharapkan", "意外参数", "想定外",
        "예상치 못함"),
];

/// Translate a canonical help block without ever touching command syntax.
pub fn help<'a>(source: &'a str, language: Language) -> Cow<'a, str> {
    if language == Language::En {
        return Cow::Borrowed(source);
    }

    let mut output = String::with_capacity(source.len().saturating_add(source.len() / 4));
    for segment in source.split_inclusive('\n') {
        let (line, newline) = segment
            .strip_suffix('\n')
            .map_or((segment, ""), |line| (line, "\n"));
        if let Some(rest) = line.strip_prefix("Usage:") {
            output.push_str(label_usage(language));
            output.push_str(usage_rest(rest, language));
        } else if let Some(rest) = line.strip_prefix("usage:") {
            output.push_str(&label_usage(language).to_lowercase());
            output.push_str(usage_rest(rest, language));
        } else if line == "  session attach <name>      start or attach to the named session" {
            output.push_str("  session attach <name>      ");
            output.push_str(text("start or attach to the named session", language));
        } else if let Some(entry) = best_suffix(line) {
            let prefix_len = line.len() - entry.en.len();
            output.push_str(&line[..prefix_len]);
            output.push_str(entry.get(language));
        } else {
            output.push_str(line);
        }
        output.push_str(newline);
    }
    Cow::Owned(output)
}

/// Translate one Luvus-owned CLI diagnostic. Unlike help rows, diagnostic
/// prose must match a catalog entry exactly so server and protocol text cannot
/// be rewritten accidentally.
pub fn diagnostic<'a>(source: &'a str, language: Language) -> Cow<'a, str> {
    let localized_help = help(source, language);
    if localized_help != source {
        return localized_help;
    }
    if language == Language::En {
        return Cow::Borrowed(source);
    }
    TEXT.iter()
        .find(|entry| entry.en == source)
        .map_or(Cow::Borrowed(source), |entry| {
            Cow::Borrowed(entry.get(language))
        })
}

fn usage_rest(rest: &str, language: Language) -> &str {
    if matches!(language, Language::Zh | Language::Ja) {
        rest.trim_start_matches(' ')
    } else {
        rest
    }
}

pub fn text(english: &'static str, language: Language) -> &'static str {
    if language == Language::En {
        return english;
    }
    HELP.iter()
        .chain(TEXT)
        .find(|entry| entry.en == english)
        .unwrap_or_else(|| panic!("missing CLI translation key: {english}"))
        .get(language)
}

/// Pad using terminal cell width, not UTF-8 bytes or Unicode scalar count.
pub fn pad(value: &str, columns: usize) -> String {
    let width = unicode_width::UnicodeWidthStr::width(value);
    let mut output = String::with_capacity(value.len() + columns.saturating_sub(width));
    output.push_str(value);
    output.extend(std::iter::repeat_n(' ', columns.saturating_sub(width)));
    output
}

fn best_suffix(line: &str) -> Option<&'static Translation> {
    HELP.iter()
        .filter(|entry| {
            if !line.as_bytes().ends_with(entry.en.as_bytes()) {
                return false;
            }
            let prefix = &line[..line.len() - entry.en.len()];
            prefix.is_empty() || prefix.chars().next_back().is_some_and(char::is_whitespace)
        })
        .max_by_key(|entry| entry.en.len())
}

fn label_usage(language: Language) -> &'static str {
    HELP.iter()
        .find(|entry| entry.en == "Usage:")
        .expect("Usage label is required")
        .get(language)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_match_the_ui_registry() {
        for code in crate::i18n::LANGS {
            assert_eq!(Language::from_code(code).code(), *code);
        }
        assert_eq!(Language::from_code("unknown"), Language::En);
    }

    #[test]
    fn canonical_syntax_is_never_rewritten() {
        let source = "Usage: luvus pane list\n  pane list    list panes\n";
        let translated = help(source, Language::Zh);
        assert!(translated.contains("luvus pane list"));
        assert!(translated.contains("pane list"));
        assert_ne!(
            help(
                "  session attach <name>      start or attach to the named session\n",
                Language::Zh,
            ),
            "  session attach <name>      start or attach to the named session\n"
        );
        assert_ne!(
            help(
                "  session attach <name>      start or attach to the named session",
                Language::Zh,
            ),
            "  session attach <name>      start or attach to the named session"
        );
        assert!(help(
            "  session attach <name>      start or attach to the named session",
            Language::Zh,
        )
        .contains("启动或连接"));
    }

    #[test]
    fn english_catalog_keys_are_unique() {
        let mut keys = HELP
            .iter()
            .chain(TEXT)
            .map(|entry| entry.en)
            .collect::<Vec<_>>();
        keys.sort_unstable();
        let duplicate = keys.windows(2).find(|pair| pair[0] == pair[1]);
        assert!(
            duplicate.is_none(),
            "duplicate CLI translation: {duplicate:?}"
        );
    }

    #[test]
    fn padding_uses_terminal_cell_width_for_cjk() {
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(pad("状态", 10).as_str()),
            10
        );
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(pad("status", 10).as_str()),
            10
        );
    }

    #[test]
    fn complete_messages_keep_locale_owned_grammar_and_punctuation() {
        let chinese = Context::for_language(Language::Zh);
        assert_eq!(
            chinese.render(
                "Luvus {latest} is available (current: {current}).",
                &[("latest", "1.2.0"), ("current", "1.1.0")],
            ),
            "Luvus 1.2.0 可用（当前版本：1.1.0）。"
        );
        assert_eq!(
            chinese.render(
                "Installed Luvus integration for {agent}.",
                &[("agent", "codex")],
            ),
            "已为 codex 安装 Luvus 集成。"
        );

        let japanese = Context::for_language(Language::Ja);
        assert_eq!(
            japanese.render(
                "Removed Luvus integration for {agent}. The agent itself was not changed.",
                &[("agent", "claude")],
            ),
            "claude 用の Luvus 連携を削除しました。エージェント自体は変更されていません。"
        );
    }

    #[test]
    fn configured_context_is_read_only_and_falls_back_safely() {
        let _env = crate::persist::test_env("cli-i18n-context");
        let home = crate::persist::config_dir();
        assert!(!home.exists());
        assert_eq!(Context::configured().language(), Language::En);
        assert!(
            !home.exists(),
            "language lookup must not create Luvus state"
        );

        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("config.json"), r#"{"language":"zh"}"#).unwrap();
        assert_eq!(Context::configured().language(), Language::Zh);

        std::fs::write(home.join("config.json"), "not json").unwrap();
        assert_eq!(Context::configured().language(), Language::En);
        std::fs::write(home.join("config.json"), r#"{"language":"unknown"}"#).unwrap();
        assert_eq!(Context::configured().language(), Language::En);
    }
}
