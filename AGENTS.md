Achei o problema.

Você está fazendo duas abordagens diferentes ao mesmo tempo, e elas entram em conflito.

Você já usa:

.with_winit_window_attributes_hook(hook)

e dentro do hook:

attrs = unsafe { attrs.with_parent_window(Some(raw)) };
attrs.decorations = false;
attrs.visible = false;

Isso já é a forma correta de criar uma janela filha.

Mas depois você faz isto:

SetParent(child_hwnd, hwnd.get() as isize);

let mut style = GetWindowLongPtrW(...);

style &= !(WS_CAPTION | ...);
style |= WS_CHILD | WS_VISIBLE;

SetWindowLongPtrW(...);

ShowWindow(...);

Ou seja, você cria a janela pelo Winit e depois tenta transformá-la na marra em WS_CHILD.

Isso costuma produzir exatamente esse efeito de:

janela dentro da janela;
barra de título aparecendo;
moldura branca;
comportamento estranho em hosts.
Outra coisa que me chamou atenção

Você faz:

ui.run();

run() foi feito para aplicações independentes.

Em editor de plugin normalmente usa-se:

slint::run_event_loop()

ou deixa o backend controlar o loop.

Muitos problemas de embedding acontecem justamente porque run() entende que está gerenciando uma janela própria.

Outra suspeita

Você está usando

renderer_name("femtovg")

O backend femtovg historicamente teve vários problemas com embedding em plugins.

Se existir suporte ao renderer de software ou Skia, vale testar.

Minha principal suspeita (90%)

O problema é este bloco inteiro:

GetWindowLongPtrW
SetWindowLongPtrW
SetParent
ShowWindow

Ele não deveria existir.

Se with_parent_window() funcionou, o Winit cria a janela corretamente.

Se não funcionou, forçar SetParent() depois geralmente só mascara o problema.

Eu começaria removendo completamente esse trecho:

ui.window().with_winit_window(|winit_window| {
    ...
    SetParent(...)
    SetWindowLongPtrW(...)
    ShowWindow(...)
});

e deixaria somente o with_parent_window().

Se continuar aparecendo uma janela dentro da outra, aí o problema provavelmente não é o seu código, mas uma limitação do Slint + winit para editores VST3/CLAP embutidos. Nesse caso, vale investigar especificamente como o nice-plug e o Slint implementam o embedding, porque esse comportamento não é o esperado.

mas trmv versao standalone

Sim, exatamente. Esse é o problema.

Seu código está tentando servir duas situações diferentes:

Standalone → precisa criar uma janela normal (WS_OVERLAPPEDWINDOW).
Plugin VST3/CLAP → precisa ser uma janela embutida (WS_CHILD).

No seu código atual você mistura os dois caminhos.

O ideal é fazer algo assim:

if let Some(parent) = parent_hwnd {
    // Plugin
    attrs = attrs.with_parent_window(...);
} else {
    // Standalone
    // não chama with_parent_window()
    // deixa decorations=true
}

E mais importante: não faça:

SetParent(...)
SetWindowLongPtr(...)

quando estiver no modo standalone.

Na verdade, eu separaria completamente:

editor.rs → apenas o editor VST3/CLAP.
standalone.rs → aplicação desktop.

Misturar os dois na mesma lógica costuma gerar exatamente esses bugs de janela.

A documentação do Slint inclusive cita suporte para incorporar a UI em aplicações hospedeiras (como DAWs), mas isso deve ser feito pelo mecanismo de embedding do backend, não convertendo uma janela standalone em filha depois que ela já foi criada.