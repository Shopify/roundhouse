//! Net::HTTP::Patch and Net HTTP exception classes resolve (stdlib surface).

use roundhouse::analyze::{diagnose, Analyzer, Severity};
use roundhouse::ingest::ingest_app;

#[test]
fn net_http_patch_and_error_classes_check_clean() {
    let root = std::env::temp_dir().join(format!("roundhouse-net-http-{}", std::process::id()));
    for (path, source) in [
        (
            "db/schema.rb",
            "ActiveRecord::Schema[8.1].define do\n  create_table :articles do |t|\n    t.string :title\n  end\nend\n",
        ),
        (
            "config/routes.rb",
            "Rails.application.routes.draw do\n  get \"/\", to: \"homes#show\"\nend\n",
        ),
        (
            "app/models/application_record.rb",
            "class ApplicationRecord < ActiveRecord::Base\n  self.abstract_class = true\nend\n",
        ),
        (
            "app/controllers/application_controller.rb",
            "class ApplicationController < ActionController::Base\nend\n",
        ),
        (
            "app/controllers/homes_controller.rb",
            "class HomesController < ApplicationController\n  NET_HTTP_ERRORS = [Net::HTTPBadResponse, Net::HTTPHeaderSyntaxError, Net::ProtocolError]\n\n  def show\n    klass = Net::HTTP::Patch\n    begin\n      raise Net::HTTPBadResponse, \"bad\"\n    rescue *NET_HTTP_ERRORS => e\n      render plain: \"#{klass.name}:#{e.class.name}\"\n    end\n  end\nend\n",
        ),
    ] {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, source).unwrap();
    }

    let mut app = ingest_app(&root).expect("ingest");
    Analyzer::new(&app).analyze(&mut app);
    let diags = diagnose(&app);
    let errors: Vec<_> = diags
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.message.clone())
        .collect();
    assert!(
        errors.is_empty(),
        "Net::HTTP::Patch and Net HTTP exceptions must resolve:\n{errors:#?}"
    );
    std::fs::remove_dir_all(root).ok();
}
