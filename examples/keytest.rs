use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers, DisableBracketedPaste, EnableBracketedPaste},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use std::io;

fn main() -> io::Result<()> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnableBracketedPaste)?;
    
    println!("Press keys to see their codes. Ctrl+C to quit.\r");
    println!("Try: Ctrl+Enter, Ctrl+J, Ctrl+M, regular Enter\r");
    println!("---\r");
    
    loop {
        if event::poll(std::time::Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) => {
                    println!("Key: code={:?} modifiers={:?}\r", key.code, key.modifiers);
                    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                        break;
                    }
                }
                Event::Paste(text) => {
                    println!("Paste: {:?} ({} chars)\r", 
                        if text.len() > 50 { &text[..50] } else { &text }, 
                        text.chars().count());
                }
                other => {
                    println!("Other: {:?}\r", other);
                }
            }
        }
    }
    
    execute!(io::stdout(), DisableBracketedPaste)?;
    disable_raw_mode()?;
    Ok(())
}
