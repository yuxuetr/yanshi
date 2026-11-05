use clap::{Args, Subcommand};

use super::{asr, tts};

#[derive(Args)]
pub struct AudioArgs {
  #[command(subcommand)]
  command: AudioCommands,
}

#[derive(Subcommand)]
enum AudioCommands {
  Asr {
    file_path: String,
    #[arg(short, long)]
    model: Option<String>,
    #[arg(short, long)]
    language: Option<String>,
    /// Q2.7.1: free-text hint forwarded to the provider as
    /// `AsrRequest.prompt`. Pre-fix this value was silently piped
    /// into the handler's positional `output` slot and the user's
    /// hint text became a file path that the CLI happily wrote to.
    #[arg(short, long)]
    prompt: Option<String>,
    /// Optional file to write the transcription to. Use `-o /tmp/x.txt`
    /// to persist the result; omit to print to stdout only.
    #[arg(short = 'o', long = "output")]
    output: Option<String>,
    #[arg(long, default_value = "text")]
    format: String,
  },
  Tts {
    input: String,
    voice: String,
    output: String,
    #[arg(short, long)]
    model: Option<String>,
    #[arg(long, default_value_t = 1.0)]
    speed: f32,
    #[arg(long, default_value = "mp3")]
    format: String,
    #[arg(long)]
    emotion: Option<String>,
  },
}

pub async fn dispatch(args: AudioArgs) -> anyhow::Result<()> {
  match args.command {
    AudioCommands::Asr {
      model,
      file_path,
      language,
      prompt,
      output,
      format,
    } => asr::execute(file_path, model, format, language, prompt, output).await,
    AudioCommands::Tts {
      model,
      voice,
      input,
      output,
      speed,
      format,
      emotion,
    } => tts::execute(input, model, voice, format, speed, output, emotion).await,
  }
}
